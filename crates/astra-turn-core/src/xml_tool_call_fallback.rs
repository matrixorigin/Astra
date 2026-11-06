//! Fallback parser for degraded tool calls in LLM text output.
//!
//! Some models emit tool calls as text instead of using the OpenAI function-calling
//! protocol (`delta.tool_calls`).  Two degradation patterns are handled:
//!
//! 1. **`<invoke>` XML**: structured XML blocks with `<parameter>` children.
//! 2. **`<tool_call>` text**: corrupted function-call syntax like
//!    `<tool_call>bash)(echo hello)` or `<tool_call>grep}{pattern: "x"}`.
//!
//! When the structured `tool_calls` array is empty but the text contains either
//! pattern, this module extracts them into the same `Vec<Value>` shape the rest
//! of the pipeline expects.
//!
//! **False-positive guard**: `<tool_call>` blocks are ambiguous, so a response
//! dominated by non-tag text is not executed. Bare `<invoke>` blocks are
//! accepted only as a terminal call suffix; the explicit DSML `tool_calls`
//! wrapper is stronger evidence and may be surrounded by prose.

use serde_json::{Value, json};
use std::ops::Range;
use std::sync::OnceLock;
use uuid::Uuid;

mod dsml;
pub use dsml::DsmlToolCallStreamFilter;

/// Maximum ratio of non-XML text to total text before we refuse to treat the
/// content as degraded `<tool_call>` blocks.  0.20 = if more than 20% of the
/// content is plain prose surrounding the tags, it's probably a normal
/// response that happens to mention the format.
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
    let calls = dsml::decode(text)
        .calls
        .into_iter()
        .filter(|call| call.format == DegradedCallFormat::Invoke)
        .map(|call| call.call)
        .collect::<Vec<_>>();
    (!calls.is_empty()).then_some(calls)
}

/// Remove explicit DSML envelopes from a complete or partial provider payload
/// before it is rendered or embedded in an interruption message.
#[must_use]
pub fn filter_dsml_tool_call_markup_for_display(text: &str) -> String {
    dsml::project_complete(text).text
}

// ─── <tool_call> Fallback ────────────────────────────────────────────────────

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
    let cleaned = rest.trim_start_matches([')', '(', '}', '{', ':', ' ']);

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
                // Use serde_json to guarantee valid JSON (escapes control chars,
                // quotes, backslashes, and non-ASCII correctly).
                return serde_json::json!({ "command": arg_content }).to_string();
            }
        }
    }

    // Last resort: if cleaned text is non-empty, wrap as command
    if !cleaned.is_empty() {
        return serde_json::json!({ "command": cleaned }).to_string();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DegradedCallFormat {
    Invoke,
    ToolCallTag,
}

#[derive(Debug, Clone)]
struct ParsedDegradedCall {
    /// Byte offset in the original provider response.  Keeping the offset
    /// here makes mixed fallback formats deterministic instead of giving one
    /// syntax family precedence over another.
    span: Range<usize>,
    format: DegradedCallFormat,
    call: Value,
    /// Explicit DSML is projected by its shared lexer; bare XML uses accepted spans.
    explicit_dsml: bool,
}

#[derive(Debug, Default)]
struct ParsedDegradedResponse {
    calls: Vec<Value>,
    /// Exact source ranges accepted as tool-call syntax. The strip path uses
    /// these same ranges, so it cannot remove a candidate rejected by the
    /// parse/ambiguity policy.
    strip_ranges: Vec<Range<usize>>,
}

fn degraded_call_identity(call: &Value) -> Option<(String, String)> {
    let function = call.get("function")?.as_object()?;
    let name = function.get("name")?.as_str()?.trim();
    let arguments = function.get("arguments")?.as_str()?.trim();
    if name.is_empty() || arguments.is_empty() {
        return None;
    }
    Some((
        name.to_string(),
        crate::stall::canonical_tool_args(arguments),
    ))
}

fn select_non_overlapping(mut candidates: Vec<ParsedDegradedCall>) -> Vec<ParsedDegradedCall> {
    // Prefer the outer block when two candidates start at the same byte. A
    // valid outer call owns any markup appearing in its argument text; nested
    // scans are never independent calls.
    candidates.sort_by(|left, right| {
        left.span
            .start
            .cmp(&right.span.start)
            .then_with(|| right.span.end.cmp(&left.span.end))
    });
    let mut selected = Vec::with_capacity(candidates.len());
    let mut last_end = 0;
    for candidate in candidates {
        if candidate.span.start < last_end {
            continue;
        }
        last_end = candidate.span.end;
        selected.push(candidate);
    }
    selected
}

fn bare_invokes_form_terminal_suffix(text: &str, candidates: &[ParsedDegradedCall]) -> bool {
    let Some(first_bare_index) = candidates
        .iter()
        .position(|call| call.format == DegradedCallFormat::Invoke && !call.explicit_dsml)
    else {
        return true;
    };
    // A normal prose prefix is allowed, but once a bare invoke begins only
    // confirmed call block may separate it from another block and from the
    // response terminus. This permits an adjacent fallback syntax while
    // preventing explanatory trailing text from being executed.
    let trailing = &candidates[first_bare_index..];
    trailing
        .windows(2)
        .all(|pair| text[pair[0].span.end..pair[1].span.start].trim().is_empty())
        && text[trailing.last().unwrap().span.end..].trim().is_empty()
}

fn remove_ranges(text: &str, ranges: &[Range<usize>]) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|range| (range.start, range.end));
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in sorted {
        if range.start < cursor || range.end > text.len() {
            continue;
        }
        output.push_str(&text[cursor..range.start]);
        cursor = range.end;
    }
    output.push_str(&text[cursor..]);
    output.trim().to_string()
}

fn parse_degraded_response(text: &str) -> ParsedDegradedResponse {
    let candidates = dsml::decode(text).calls;
    let bare_suffix = bare_invokes_form_terminal_suffix(text, &candidates);
    let accepted_invokes: Vec<ParsedDegradedCall> = candidates
        .iter()
        .filter(|invoke| {
            invoke.format == DegradedCallFormat::Invoke && (invoke.explicit_dsml || bare_suffix)
        })
        .cloned()
        .collect();

    let accepted_invoke_ranges: Vec<Range<usize>> = accepted_invokes
        .iter()
        .map(|invoke| invoke.span.clone())
        .collect();
    let mut tool_tags = candidates
        .into_iter()
        .filter(|candidate| candidate.format == DegradedCallFormat::ToolCallTag)
        .collect::<Vec<_>>();

    // Apply the ambiguity guard only to residual text outside accepted invoke
    // blocks. A large structured block must not subsidize prose around an
    // unrelated, ambiguous tag.
    let residual_total = text.len().saturating_sub(
        accepted_invoke_ranges
            .iter()
            .map(|range| range.end.saturating_sub(range.start))
            .sum::<usize>(),
    );
    let tag_bytes = tool_tags
        .iter()
        .map(|tag| tag.span.end.saturating_sub(tag.span.start))
        .sum::<usize>();
    if residual_total > 0
        && !tool_tags.is_empty()
        && residual_total.saturating_sub(tag_bytes) as f64 / residual_total as f64
            > MAX_NON_XML_RATIO
    {
        tool_tags.clear();
    }

    let mut parsed = accepted_invokes;
    parsed.extend(tool_tags);
    let parsed = select_non_overlapping(parsed);
    if parsed.is_empty() {
        return ParsedDegradedResponse::default();
    }

    let mut calls = Vec::with_capacity(parsed.len());
    // One occurrence can cancel at most one occurrence from the other syntax
    // family. Same-family duplicates remain intact.
    let mut seen_identities =
        std::collections::HashMap::<(DegradedCallFormat, (String, String)), usize>::new();
    for entry in &parsed {
        let mut duplicate = false;
        if let Some(identity) = degraded_call_identity(&entry.call) {
            let other = match entry.format {
                DegradedCallFormat::Invoke => DegradedCallFormat::ToolCallTag,
                DegradedCallFormat::ToolCallTag => DegradedCallFormat::Invoke,
            };
            let other_count = seen_identities
                .get(&(other, identity.clone()))
                .copied()
                .unwrap_or(0);
            let occurrence = seen_identities.entry((entry.format, identity)).or_default();
            *occurrence += 1;
            duplicate = other_count >= *occurrence;
        }
        if !duplicate {
            calls.push(entry.call.clone());
        }
    }

    // Keep DSML bytes for the shared visible-text lexer. Removing only the
    // invocation first would strand wrappers and trim surrounding answer bytes.
    let strip_ranges = parsed
        .iter()
        .filter(|call| !call.explicit_dsml)
        .map(|call| call.span.clone())
        .collect();

    ParsedDegradedResponse {
        calls,
        strip_ranges,
    }
}

/// Unified fallback for provider responses that contain only degraded text.
///
/// Native structured calls are handled by the caller before this function.
/// When fallback is needed, all independently confirmed syntax families are
/// merged in source order.  A duplicate represented once as `<invoke>` and
/// once as `<tool_call>` is executed once; two calls from the same syntax
/// family are preserved because they may be intentional repeated operations.
pub fn parse_degraded_tool_calls(text: &str) -> Option<Vec<Value>> {
    let parsed = parse_degraded_response(text);
    (!parsed.calls.is_empty()).then_some(parsed.calls)
}

/// Unified strip: remove exactly the ranges accepted by the unified parser.
/// Keeping parse and strip on one canonical decision prevents a rejected
/// explanatory block from being silently deleted just because another format
/// was accepted in the same response.
pub fn strip_degraded_tool_calls(text: &str) -> String {
    let parsed = parse_degraded_response(text);
    let after_accepted = remove_ranges(text, &parsed.strip_ranges);
    // DSML owns its parameter bytes. Remove protocol spans before the legacy
    // bare-XML cleanup can interpret markup inside an argument as answer text.
    let projection = dsml::project_complete(&after_accepted);
    let text = if let Some(start) =
        truncated_degraded_tool_call_tail_start(&projection.text, &projection.literal)
    {
        projection.text[..start].trim_end()
    } else {
        &projection.text
    };
    strip_residual_outside_literals(text, &projection.literal)
}

fn in_literal(ranges: &[Range<usize>], pos: usize) -> bool {
    ranges
        .get(ranges.partition_point(|range| range.end <= pos))
        .is_some_and(|range| range.contains(&pos))
}

fn truncated_degraded_tool_call_tail_start(text: &str, literal: &[Range<usize>]) -> Option<usize> {
    ["<invoke", "<tool_call"]
        .into_iter()
        .filter_map(|marker| text.rfind(marker).map(|pos| (marker, pos)))
        .filter(|(marker, pos)| {
            if in_literal(literal, *pos) {
                return false;
            }
            let tail = &text[*pos..];
            let closing = if *marker == "<invoke" {
                "</invoke>"
            } else {
                "</tool_call>"
            };
            !tail.contains(closing)
                && tag_starts_tool_block_line(text, *pos)
                && tail_looks_like_tool_block(tail)
        })
        .map(|(_, pos)| pos)
        .min()
}

fn tag_starts_tool_block_line(text: &str, pos: usize) -> bool {
    text[..pos]
        .rsplit_once('\n')
        .map(|(_, line)| line.trim().is_empty())
        .unwrap_or_else(|| text[..pos].trim().is_empty())
}

fn tail_looks_like_tool_block(tail: &str) -> bool {
    let first_line = tail.lines().next().unwrap_or(tail).trim_start();
    (first_line.starts_with("<invoke")
        && (first_line.contains("name=") || tail.contains("<parameter")))
        || (first_line.starts_with("<tool_call") && tail.contains("<function"))
}

/// Strip bare `<parameter=…>` and `<function=…>` fragments that aren't
/// wrapped in proper `<invoke>` blocks. These appear when models emit
/// degraded tool call syntax directly in text output (no `<invoke>` wrapper,
/// no valid XML quoting — just `<parameter=action> value`-style raw text).
///
/// Without this cleanup, the user sees raw tool-call markup leaking into the
/// transcript alongside the assistant's natural-language response.
#[cfg(test)]
fn strip_residual_xml_fragments(text: &str) -> String {
    strip_residual_outside_literals(text, &[])
}

fn strip_residual_outside_literals(text: &str, literal: &[Range<usize>]) -> String {
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut changed = false;
    let mut append = |run: &str, output: &mut String| {
        if let Some(clean) = cleanup_residual_run(run) {
            changed = true;
            output.push_str(&clean);
        } else {
            output.push_str(run);
        }
    };
    for range in literal {
        if range.start >= text.len() {
            break;
        }
        append(&text[cursor..range.start], &mut output);
        let end = range.end.min(text.len());
        output.push_str(&text[range.start..end]);
        cursor = end;
    }
    append(&text[cursor..], &mut output);
    if changed {
        output.trim().to_owned()
    } else {
        output
    }
}

fn cleanup_residual_run(text: &str) -> Option<String> {
    use regex::Regex;

    if !text.contains("<parameter=") && !text.contains("<function=") {
        return None;
    }

    // `<parameter=KEY>` or `<parameter=KEY>VALUE` — strip tag and the
    // immediately-following word (the "value") when present on the same line.
    static RE_PARAM: OnceLock<Regex> = OnceLock::new();
    let re_param = RE_PARAM.get_or_init(|| Regex::new(r"<parameter=[^>]+>\s*\S*").unwrap());

    // `<function=NAME>` — self-closing fragment, no value.
    static RE_FUNC: OnceLock<Regex> = OnceLock::new();
    let re_func = RE_FUNC.get_or_init(|| Regex::new(r"<function=[^>]+>").unwrap());

    if !re_param.is_match(text) && !re_func.is_match(text) {
        return None;
    }
    let mut result = re_param.replace_all(text, "").to_string();
    result = re_func.replace_all(&result, "").to_string();

    // Collapse multiple spaces left by removal, trim per-line.
    static RE_SPACES: OnceLock<Regex> = OnceLock::new();
    let re_spaces = RE_SPACES.get_or_init(|| Regex::new(r" {2,}").unwrap());
    result = re_spaces.replace_all(&result, " ").to_string();

    // Remove blank lines created by fragment removal.
    static RE_BLANK: OnceLock<Regex> = OnceLock::new();
    let re_blank = RE_BLANK.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    result = re_blank.replace_all(&result, "\n\n").to_string();

    Some(result)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsml_orphan_closers_are_not_calls_or_answer_text() {
        let answer = "Mailbox permission is required.";
        let text =
            format!("{answer}</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>");
        assert_eq!(strip_degraded_tool_calls(&text), answer);
        assert!(parse_degraded_tool_calls(&text).is_none());
        let mut filter = DsmlToolCallStreamFilter::default();
        let mut visible = String::new();
        for ch in text.chars() {
            visible.push_str(&filter.push(&ch.to_string()));
        }
        visible.push_str(&filter.finish());
        assert_eq!(visible, answer);
    }

    #[test]
    fn dsml_calls_support_typed_arguments_and_preserve_surrounding_answer() {
        for prefix in ["｜DSML｜", "｜｜DSML｜｜", "|DSML|", "||DSML||"] {
            let text = format!(
                "Before.<{prefix} calls><{prefix} invoke name=\"search\"><{prefix} parameter name=\"query\" string=\"true\">123</{prefix} parameter><{prefix} parameter name=\"count\" string=\"false\">5</{prefix} parameter></{prefix} invoke></{prefix} calls>After."
            );
            let calls = parse_degraded_tool_calls(&text).expect("recover declared DSML invocation");
            assert_eq!(calls[0]["function"]["name"], "search");
            let args: Value =
                serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(args, json!({"query":"123", "count":5}));
            assert_eq!(strip_degraded_tool_calls(&text), "Before.After.");
        }
    }

    #[test]
    fn dsml_examples_remain_literal_in_markdown() {
        for text in [
            "Example: `</｜｜DSML｜｜ calls>` is a closing tag.",
            "```xml\n<｜｜DSML｜｜tool_calls><｜｜DSML｜｜invoke name=\"bash\"/></｜｜DSML｜｜tool_calls>\n```",
            "```text\n```not-a-closing-fence\n</｜DSML｜ calls>\n```",
        ] {
            assert_eq!(strip_degraded_tool_calls(text), text);
            assert!(parse_degraded_tool_calls(text).is_none());
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut visible = String::new();
            for ch in text.chars() {
                visible.push_str(&filter.push(&ch.to_string()));
            }
            visible.push_str(&filter.finish());
            assert_eq!(visible, text);
        }
    }

    #[test]
    fn dsml_nested_markup_cannot_become_a_second_call() {
        let text = "<｜DSML｜ calls><｜DSML｜ invoke name=\"write_file\">broken <｜DSML｜ invoke name=\"bash\">{\"command\":\"pwd\"}</｜DSML｜ invoke></｜DSML｜ invoke></｜DSML｜ calls>";
        let calls = parse_degraded_tool_calls(text).expect("retain invalid outer call");
        assert_eq!(
            calls
                .iter()
                .map(|call| call["function"]["name"].as_str())
                .collect::<Vec<_>>(),
            vec![Some("write_file")]
        );
        assert!(
            serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap())
                .is_err()
        );
        assert_eq!(strip_degraded_tool_calls(text), "");
    }

    #[test]
    fn dsml_damaged_arguments_never_become_an_empty_executable_call() {
        for text in [
            "<｜DSML｜ calls><｜DSML｜ invoke name=\"search\"><｜DSML｜ parameter name=\"count\" string=\"false\">not-json</｜DSML｜ parameter></｜DSML｜ invoke></｜DSML｜ calls>",
            "<｜DSML｜ calls><｜DSML｜ invoke name=\"search\"><｜DSML｜ parameter name=\"query\" string=\"true\">unfinished",
        ] {
            let calls = parse_degraded_tool_calls(text)
                .expect("retain named failed call for existing admission feedback");
            assert_eq!(calls[0]["function"]["name"], "search");
            assert!(
                serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap())
                    .is_err()
            );
            assert_eq!(strip_degraded_tool_calls(text), "");
        }
    }

    #[test]
    fn dsml_invalid_outer_subtree_cannot_release_nested_invocations() {
        for (text, outer) in [
            (
                r#"<||DSML||tool_calls><||DSML||invoke name=""><||DSML||invoke name="bash"><||DSML||parameter name="command">pwd</||DSML||parameter></||DSML||invoke></||DSML||invoke></||DSML||tool_calls>"#,
                None,
            ),
            (
                r#"<||DSML||tool_calls><||DSML||invoke><||DSML||invoke name="bash">{"command":"pwd"}</||DSML||invoke></||DSML||invoke></||DSML||tool_calls>"#,
                None,
            ),
            (
                r#"<｜DSML｜ calls><｜DSML｜ invoke name="write_file"><｜DSML｜ calls></｜DSML｜ calls><｜DSML｜ invoke name="bash">{"command":"pwd"}</｜DSML｜ invoke></｜DSML｜ invoke></｜DSML｜ calls>"#,
                Some("write_file"),
            ),
        ] {
            let calls = parse_degraded_tool_calls(text).unwrap_or_default();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["function"]["name"].as_str())
                    .collect::<Vec<_>>(),
                outer.into_iter().map(Some).collect::<Vec<_>>()
            );
            for call in &calls {
                assert!(
                    serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap())
                        .is_err()
                );
            }
            assert_eq!(strip_degraded_tool_calls(text), "");
        }
    }

    #[test]
    fn dsml_container_and_indented_code_are_literal() {
        use pulldown_cmark::{Event, Parser, Tag, TagEnd};
        let call = r#"<｜DSML｜ invoke name="bash">{"command":"pwd"}</｜DSML｜ invoke>"#;
        for text in [
            format!("- ~~~xml\n\n  {call}\n  ~~~\n"),
            format!("1. ~~~xml\n\n   {call}\n   ~~~\n"),
            format!("> ~~~text\n> {call}\n> ~~~"),
            format!("    {call}\n"),
            format!("- ~~~text\n  {call}\n  ~~~"),
            format!("1. ~~~text\n   {call}\n   ~~~"),
            format!("> - ~~~text\n>   {call}\n>   ~~~"),
            format!("100. ~~~text\n     {call}\n     ~~~"),
            format!("```text\n> ```\n{call}\n```"),
            format!("> - ~~~xml\n>\n>   {call}\n>   ~~~\n"),
            format!(">\t~~~xml\n> {call}\n> ~~~\n"),
            format!("\t{call}\n"),
        ]
        .into_iter()
        .flat_map(|text| [text.clone(), text.replace("~~~", "```")])
        .flat_map(|text| ["\n", "\r\n"].map(|ending| text.replace('\n', ending)))
        {
            let mut code = false;
            assert!(
                Parser::new(&text).any(|event| match event {
                    Event::Start(Tag::CodeBlock(_)) => {
                        code = true;
                        false
                    }
                    Event::End(TagEnd::CodeBlock) => {
                        code = false;
                        false
                    }
                    Event::Text(value) => code && value.contains(&call),
                    _ => false,
                }),
                "CommonMark must classify the invocation as code: {text:?}"
            );
            assert!(
                parse_degraded_tool_calls(&text).is_none(),
                "code example: {text:?}"
            );
            assert_eq!(strip_degraded_tool_calls(&text), text);
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut visible = String::new();
            for ch in text.chars() {
                visible.push_str(&filter.push(&ch.to_string()));
            }
            visible.push_str(&filter.finish());
            assert_eq!(visible, text);
        }
    }

    #[test]
    fn dsml_calls_after_markdown_container_end_are_recovered() {
        let example = r#"<｜DSML｜ invoke name="example">{}</｜DSML｜ invoke>"#;
        let real = r#"<｜DSML｜ invoke name="bash">{"command":"pwd"}</｜DSML｜ invoke>"#;
        for text in [
            format!("100. ~~~text\n     {example}\n     ~~~\n{real}"),
            format!("> ~~~text\n> {example}\n{real}"),
            format!("- ~~~text\n  {example}\n{real}"),
        ]
        .into_iter()
        .flat_map(|text| ["\n", "\r\n"].map(|ending| text.replace('\n', ending)))
        {
            let calls = parse_degraded_tool_calls(&text).unwrap();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["function"]["name"].as_str())
                    .collect::<Vec<_>>(),
                vec![Some("bash")]
            );
            let projected = strip_degraded_tool_calls(&text);
            assert!(projected.contains(example));
            assert!(!projected.contains(real));
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut visible = String::new();
            for ch in text.chars() {
                visible.push_str(&filter.push(&ch.to_string()));
            }
            visible.push_str(&filter.finish());
            assert_eq!(visible, projected);
        }
    }

    #[test]
    fn dsml_parameter_body_is_not_legacy_xml_cleanup_input() {
        let text = r#"Before.<||DSML||tool_calls><||DSML||invoke name="write_file"><||DSML||parameter name="path">example.txt</||DSML||parameter><||DSML||parameter name="content"><parameter=x>hello</||DSML||parameter></||DSML||invoke></||DSML||tool_calls>After."#;
        assert_eq!(strip_degraded_tool_calls(text), "Before.After.");
    }

    #[test]
    fn dsml_inline_code_closes_on_an_indented_continuation() {
        let real = r#"<|DSML|invoke name="bash">{"command":"pwd"}</|DSML|invoke>"#;
        let text = format!("Before `foo\n    bar`\n{real}");
        let calls =
            parse_degraded_tool_calls(&text).expect("real invocation after closed inline code");
        assert_eq!(calls[0]["function"]["name"], "bash");
        assert_eq!(strip_degraded_tool_calls(&text), "Before `foo\n    bar`\n");
        let mut filter = DsmlToolCallStreamFilter::default();
        let mut visible = String::new();
        for ch in text.chars() {
            visible.push_str(&filter.push(&ch.to_string()));
        }
        visible.push_str(&filter.finish());
        assert_eq!(visible, strip_degraded_tool_calls(&text));
    }

    #[test]
    fn dsml_non_protocol_tag_names_remain_text_for_every_chunking() {
        for text in [
            r#"<|DSML|invoke-example name="bash">{"command":"pwd"}</|DSML|invoke-example>"#,
            r#"<|DSML|calls-example><|DSML|parameter-example name="command">pwd</|DSML|parameter-example></|DSML|calls-example>"#,
        ] {
            assert!(parse_degraded_tool_calls(text).is_none());
            assert_eq!(strip_degraded_tool_calls(text), text);
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut visible = String::new();
            for ch in text.chars() {
                visible.push_str(&filter.push(&ch.to_string()));
            }
            visible.push_str(&filter.finish());
            assert_eq!(visible, text);
        }
    }

    #[test]
    fn dsml_markdown_block_lifetimes_and_escaped_examples() {
        let call = r#"<|DSML|invoke name="read_file">{"path":"README.md"}</|DSML|invoke>"#;
        for (text, visible, executable) in [
            (
                format!("# Unmatched ` delimiter\n{call}"),
                "# Unmatched ` delimiter\n".to_owned(),
                true,
            ),
            (
                "Escaped partial: \\<|DSML|invoke name=\"".to_owned(),
                "Escaped partial: \\<|DSML|invoke name=\"".to_owned(),
                false,
            ),
            (
                format!("The literal is: \\{call}"),
                format!("The literal is: \\{call}"),
                false,
            ),
            (
                format!("Use a ` character.\n\n`{call}`"),
                format!("Use a ` character.\n\n`{call}`"),
                false,
            ),
            (
                format!("Use a ` character.\n\n{call}"),
                "Use a ` character.\n\n".to_owned(),
                true,
            ),
            (
                format!("- Example\n  ~~~text\n  example\n~~~\n{call}\n~~~\n"),
                format!("- Example\n  ~~~text\n  example\n~~~\n{call}\n~~~\n"),
                false,
            ),
            (
                format!("- Example\n  ~~~text\n  example\n{call}"),
                "- Example\n  ~~~text\n  example\n".to_owned(),
                true,
            ),
        ] {
            let calls = parse_degraded_tool_calls(&text).unwrap_or_default();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["function"]["name"].as_str())
                    .collect::<Vec<_>>(),
                if executable {
                    vec![Some("read_file")]
                } else {
                    vec![]
                },
                "{text:?}"
            );
            assert_eq!(strip_degraded_tool_calls(&text), visible);
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut published = String::new();
            for ch in text.chars() {
                published.push_str(&filter.push(&ch.to_string()));
            }
            published.push_str(&filter.finish());
            assert_eq!(published, visible);
        }
    }

    #[test]
    fn dsml_commonmark_code_blocks_and_spans_are_data() {
        use pulldown_cmark::{Event, Parser, Tag, TagEnd};
        let call = r#"<|DSML|invoke name="read_file">{"path":"README.md"}</|DSML|invoke>"#;
        for text in [
            format!("Use a ` character\n===\n`{call}`"),
            format!("Text\n    `example\n{call}`"),
            format!("Text\n\n    `example\n    {call}`\n"),
            format!("Use a ` character.\n\n`{call}`"),
            format!("Use a ` character.\n# Heading\n`{call}`"),
            format!("# Unmatched ` delimiter\n`{call}`"),
            format!("- Example\ncontinued\n  ~~~text\n  example\n~~~\n{call}\n~~~\n"),
            format!("- Example\n\n  ~~~text\n  example\n~~~\n{call}\n~~~\n"),
            format!("> - Example\n>   ~~~text\n>   example\n> ~~~\n> {call}\n> ~~~\n"),
        ] {
            let mut code = String::new();
            let mut block = false;
            for event in Parser::new(&text) {
                match event {
                    Event::Start(Tag::CodeBlock(_)) => block = true,
                    Event::End(TagEnd::CodeBlock) => block = false,
                    Event::Code(text) => code.push_str(&text),
                    Event::Text(text) if block => code.push_str(&text),
                    _ => {}
                }
            }
            assert!(
                code.contains(call),
                "fixture must render the invocation as CommonMark code: {text:?}"
            );
            assert!(
                parse_degraded_tool_calls(&text).is_none(),
                "literal was executed: {text:?}"
            );
            assert_eq!(strip_degraded_tool_calls(&text), text);
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut visible = String::new();
            for ch in text.chars() {
                visible.push_str(&filter.push(&ch.to_string()));
            }
            visible.push_str(&filter.finish());
            assert_eq!(visible, text);
        }
    }

    #[test]
    fn dsml_paragraph_and_escaped_scope_boundaries_preserve_real_calls() {
        let call = r#"<|DSML|invoke name="read_file">{"path":"README.md"}</|DSML|invoke>"#;
        for (text, visible, executable) in [
            (
                format!("Use a ` character\n===\n`{call}`"),
                format!("Use a ` character\n===\n`{call}`"),
                false,
            ),
            (
                format!("Use a ` character\n===\n{call}"),
                "Use a ` character\n===\n".to_owned(),
                true,
            ),
            (
                format!("Text\n    `example\n{call}`"),
                format!("Text\n    `example\n{call}`"),
                false,
            ),
            (
                format!("Text\n\n    `example\n    {call}`\n"),
                format!("Text\n\n    `example\n    {call}`\n"),
                false,
            ),
            (
                format!("Example opener: \\<|DSML|calls>\n\n<|DSML|calls>{call}</|DSML|calls>"),
                "Example opener: \\<|DSML|calls>\n\n".to_owned(),
                true,
            ),
            (
                format!(
                    "Example opener: \\<|DSML|invoke name=\"read_file\">\n\n<|DSML|calls>{call}</|DSML|calls>"
                ),
                "Example opener: \\<|DSML|invoke name=\"read_file\">\n\n".to_owned(),
                true,
            ),
        ] {
            let calls = parse_degraded_tool_calls(&text).unwrap_or_default();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["function"]["name"].as_str())
                    .collect::<Vec<_>>(),
                if executable {
                    vec![Some("read_file")]
                } else {
                    vec![]
                },
                "{text:?}"
            );
            assert_eq!(strip_degraded_tool_calls(&text), visible);
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut published = String::new();
            for ch in text.chars() {
                published.push_str(&filter.push(&ch.to_string()));
            }
            published.push_str(&filter.finish());
            assert_eq!(published, visible);
        }
    }

    #[test]
    fn degraded_markup_never_promotes_literal_or_owned_nested_calls() {
        let call = r#"<|DSML|invoke name="bash">{"command":"pwd"}</|DSML|invoke>"#;
        let legacy = "<tool_call>bash)(pwd)</tool_call>";
        for text in [
            format!("<pre>\n{call}\n</pre>\n"),
            format!("<!-- {call} -->"),
            format!("<code>{call}</code>"),
            format!("<invoke name=\"\">{call}</invoke>"),
            format!("<parameter name=\"example\">{call}</parameter>"),
            format!("<|DSML|parameter name=\"example\">{legacy}</|DSML|parameter>"),
            format!("<|DSML|calls>{legacy}</|DSML|calls>"),
        ] {
            assert!(
                parse_degraded_tool_calls(&text).is_none(),
                "nested example executed: {text:?}"
            );
        }
    }

    #[test]
    fn degraded_invocations_keep_their_own_opening_attributes() {
        let inner =
            r#"<|DSML|invoke name="bash"><parameter name="command">pwd</parameter></|DSML|invoke>"#;
        for (open, close) in [
            ("<Invoke", "</Invoke>"),
            ("< invoke", "</invoke>"),
            ("<invoke", "</invoke>"),
        ] {
            for attributes in [
                "",
                " name=\"\"",
                " label='name=\"bash\"'",
                " tool_name=\"bash\"",
                " name=\"bash\" name=\"read_file\"",
            ] {
                let text = format!("{open}{attributes}>{inner}{close}");
                assert!(parse_degraded_tool_calls(&text).is_none(), "{text:?}");
            }
            let text =
                format!("{open} name=\"bash\"><parameter name=\"command\">pwd</parameter>{close}");
            let calls = parse_degraded_tool_calls(&text).unwrap();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| (
                        call["function"]["name"].clone(),
                        serde_json::from_str::<Value>(
                            call["function"]["arguments"].as_str().unwrap()
                        )
                        .unwrap()
                    ))
                    .collect::<Vec<_>>(),
                vec![(json!("bash"), json!({"command":"pwd"}))]
            );
            let text = format!("{open} name=\"read_file\">{inner}{close}");
            let calls = parse_degraded_tool_calls(&text).unwrap();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["function"]["name"].as_str())
                    .collect::<Vec<_>>(),
                vec![Some("read_file")]
            );
            assert!(
                serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap())
                    .is_err(),
                "invalid nested body must reach argument rejection"
            );
        }
        let text = r#"<invoke name="read_file"><parameter name="path" string="false">123</parameter></invoke>"#;
        let calls = parse_degraded_tool_calls(text).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap())
                .unwrap(),
            json!({"path":"123"})
        );
    }

    #[test]
    fn dsml_closed_inline_code_at_line_start_does_not_own_later_calls() {
        use pulldown_cmark::{Event, Parser};
        let call = r#"<|DSML|invoke name="bash">{"command":"pwd"}</|DSML|invoke>"#;
        for prefix in [
            "```example```\n",
            "``example``\n",
            "Text ```example```\n",
            "````a`b````\n",
            "> ```example```\n",
            "- ```example```\n",
        ] {
            assert!(
                Parser::new(prefix).any(|event| matches!(event, Event::Code(_))),
                "{prefix:?}"
            );
            let text = format!("{prefix}{call}");
            let calls = parse_degraded_tool_calls(&text).expect("call after closed inline span");
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["function"]["name"].as_str())
                    .collect::<Vec<_>>(),
                vec![Some("bash")]
            );
            assert_eq!(strip_degraded_tool_calls(&text), prefix);
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut published = String::new();
            for ch in text.chars() {
                published.push_str(&filter.push(&ch.to_string()));
            }
            published.push_str(&filter.finish());
            assert_eq!(published, prefix);
        }
    }

    #[test]
    fn degraded_markup_html_lifetimes_preserve_later_real_calls() {
        let call = r#"<|DSML|invoke name="bash">{"command":"pwd"}</|DSML|invoke>"#;
        for (prefix, literal) in [
            ("<pre>\nexample\n</pre>\n", false),
            ("<!--\nexample\n-->\n", false),
            ("<pre>\nexample\n</pre>\n    ", true),
            ("<!--\nexample\n-->\n    ", true),
            ("<div>\nexample\n</div>\n", true),
            ("<div>\nexample\n</div>\n\n", false),
            ("> <pre>\n> example\n", false),
            ("> <!--\n> example\n", false),
            ("Answer <code>example</code>. ", false),
            ("Answer <br>", false),
        ] {
            let text = format!("{prefix}{call}");
            let calls = parse_degraded_tool_calls(&text).unwrap_or_default();
            let actual = calls
                .iter()
                .map(|call| call["function"]["name"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                actual,
                if literal { vec![] } else { vec![Some("bash")] },
                "{text:?}"
            );
            let expected = if literal {
                text.clone()
            } else {
                prefix.to_owned()
            };
            assert_eq!(strip_degraded_tool_calls(&text), expected);
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut visible = String::new();
            for ch in text.chars() {
                visible.push_str(&filter.push(&ch.to_string()));
            }
            visible.push_str(&filter.finish());
            assert_eq!(visible, expected);
        }
        for text in [
            "<!--\n<invoke name=\"bash\">\n-->",
            "Text <code><parameter=example> pwd</code>.",
        ] {
            assert_eq!(strip_degraded_tool_calls(text), text);
        }
    }

    #[test]
    fn degraded_markup_mixed_unrecoverable_scopes_remain_bounded() {
        for inner in ["<invoke/>", "<tool_call/>"] {
            for pairs in [8_000, 16_000, 32_000] {
                let text = format!("{}{}", "<|DSML|invoke/>".repeat(pairs), inner.repeat(pairs));
                let started = std::time::Instant::now();
                assert!(parse_degraded_tool_calls(&text).is_none());
                eprintln!(
                    "mixed malformed {inner} scopes {pairs}: {:?}",
                    started.elapsed()
                );
            }
        }
    }

    #[test]
    fn dsml_long_fence_info_preserves_literal_output_across_chunking() {
        use pulldown_cmark::{Event, Parser, Tag};
        for info in ["x`", "xy"] {
            for repeats in [8_000, 16_000, 32_000] {
                let text = format!("~~~{}\nbody\n~~~\n", info.repeat(repeats));
                assert!(matches!(
                    Parser::new(&text).next(),
                    Some(Event::Start(Tag::CodeBlock(_)))
                ));
                let started = std::time::Instant::now();
                assert!(parse_degraded_tool_calls(&text).is_none());
                let decoded = started.elapsed();
                assert_eq!(filter_dsml_tool_call_markup_for_display(&text), text);
                for width in [1, 16] {
                    let mut filter = DsmlToolCallStreamFilter::default();
                    let mut visible = String::new();
                    for chunk in text.as_bytes().chunks(width) {
                        visible.push_str(&filter.push(std::str::from_utf8(chunk).unwrap()));
                    }
                    visible.push_str(&filter.finish());
                    assert_eq!(visible, text);
                }
                eprintln!(
                    "fence info {info:?} {repeats}: decode {decoded:?}, all projections {:?}",
                    started.elapsed()
                );
            }
        }
    }

    #[test]
    fn dsml_newlines_inside_tags_do_not_restart_line_indentation() {
        let text = "one</|DSML|\ncalls>    two</|DSML|calls>three";
        assert_eq!(strip_degraded_tool_calls(text), "one    twothree");
        let mut filter = DsmlToolCallStreamFilter::default();
        let mut published = String::new();
        for ch in text.chars() {
            published.push_str(&filter.push(&ch.to_string()));
        }
        published.push_str(&filter.finish());
        assert_eq!(published, "one    twothree");
    }

    #[test]
    fn dsml_adversarial_unmatched_closers_are_linear_and_not_calls() {
        let text = format!(
            "{}{}",
            "<|DSML|calls>".repeat(100_000),
            "</|DSML|invoke>".repeat(100_000)
        );
        let started = std::time::Instant::now();
        assert!(parse_degraded_tool_calls(&text).is_none());
        assert_eq!(strip_degraded_tool_calls(&text), "");
        let mut filter = DsmlToolCallStreamFilter::default();
        let mut published = String::new();
        for chunk in text.as_bytes().chunks(47) {
            published.push_str(&filter.push(std::str::from_utf8(chunk).unwrap()));
        }
        published.push_str(&filter.finish());
        assert_eq!(published, "");
        eprintln!(
            "adversarial nested scopes {} bytes: {:?}",
            text.len(),
            started.elapsed()
        );
    }

    #[test]
    fn dsml_fragmented_long_prefixes_preserve_their_semantics() {
        for (text, expected) in [
            (" ".repeat(512 * 1024), " ".repeat(512 * 1024)),
            ("> ".repeat(128 * 1024), "> ".repeat(128 * 1024)),
            (
                format!(
                    "{}first.\n{}",
                    "> ".repeat(32_000),
                    "continuation\n".repeat(4096)
                ),
                format!(
                    "{}first.\n{}",
                    "> ".repeat(32_000),
                    "continuation\n".repeat(4096)
                ),
            ),
            ("`".repeat(512 * 1024), "`".repeat(512 * 1024)),
            (
                format!("```text\n```{}\n", " ".repeat(512 * 1024)),
                format!("```text\n```{}\n", " ".repeat(512 * 1024)),
            ),
            (
                format!("<|DSML|invoke name=\"{}", "a".repeat(512 * 1024)),
                String::new(),
            ),
        ] {
            let started = std::time::Instant::now();
            let mut filter = DsmlToolCallStreamFilter::default();
            let mut visible = String::new();
            for chunk in text.as_bytes().chunks(16) {
                visible.push_str(&filter.push(std::str::from_utf8(chunk).unwrap()));
            }
            visible.push_str(&filter.finish());
            assert_eq!(visible, expected);
            assert_eq!(filter_dsml_tool_call_markup_for_display(&text), expected);
            eprintln!(
                "fragmented prefix {} bytes: {:?}",
                text.len(),
                started.elapsed()
            );
        }
    }

    #[test]
    fn dsml_long_multiline_answer_preserves_text() {
        for text in [
            "Ordinary model answer with multiple words and no protocol markup on this line.\n"
                .repeat(55_000),
            "<a".repeat(250_000),
        ] {
            let started = std::time::Instant::now();
            assert!(parse_degraded_tool_calls(&text).is_none());
            assert_eq!(strip_degraded_tool_calls(&text), text);
            eprintln!(
                "DSML decode and projection: {} bytes in {:?}",
                text.len(),
                started.elapsed()
            );
        }
    }

    #[test]
    fn dsml_stream_filter_hides_envelope_across_chunk_boundaries() {
        let mut filter = DsmlToolCallStreamFilter::default();
        let chunks = [
            "visible before <｜｜DS",
            "ML｜｜tool_calls><｜｜DSML｜｜invoke name=\"bash\">secret",
            "</｜｜DSML｜｜invoke></｜｜DSML",
            "｜｜tool_calls> visible after",
        ];
        let mut visible = chunks
            .into_iter()
            .map(|chunk| filter.push(chunk))
            .collect::<String>();
        visible.push_str(&filter.finish());

        assert_eq!(visible, "visible before  visible after");
        assert!(!visible.contains("DSML"));
        assert!(!visible.contains("secret"));
    }

    #[test]
    fn dsml_stream_filter_hides_ascii_and_unfinished_envelopes() {
        let complete = "before< ||DSML||tool_calls >< ||DSML||invoke name=\"bash\">hidden</ ||DSML||invoke></ ||DSML||tool_calls >after";
        assert_eq!(
            filter_dsml_tool_call_markup_for_display(complete),
            "beforeafter"
        );

        let incomplete =
            "safe prefix<｜｜DSML｜｜tool_calls><｜｜DSML｜｜invoke name=\"bash\">hidden";
        assert_eq!(
            filter_dsml_tool_call_markup_for_display(incomplete),
            "safe prefix"
        );
    }

    #[test]
    fn dsml_stream_filter_preserves_non_protocol_angle_brackets() {
        assert_eq!(
            filter_dsml_tool_call_markup_for_display("explain <tool_calls> literally"),
            "explain <tool_calls> literally"
        );
        assert_eq!(filter_dsml_tool_call_markup_for_display("2 <"), "2 <");
    }

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
    fn parse_dsml_wrapped_invoke_with_params() {
        let dsml = concat!(
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}tool_calls>",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke name=\"agent\">",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter name=\"action\" string=\"true\">get_result",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter>",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter name=\"agent_id\" string=\"true\">reviewer@run",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter>",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke>",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}tool_calls>",
        );

        let calls = parse_xml_tool_calls(dsml).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "agent");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["action"], "get_result");
        assert_eq!(args["agent_id"], "reviewer@run");
    }

    #[test]
    fn parse_fullwidth_dsml_with_adjacent_tool_call_blocks() {
        let dsml = concat!(
            "Let me start:\n\n",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}tool_calls>\n",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke name=\"bash\">\n",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter name=\"command\" string=\"true\">echo test</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter>\n",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke>\n",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}tool_calls><\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}tool_calls>\n",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke name=\"bash\">\n",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter name=\"command\" string=\"true\">mkdir -p /run/sshd</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter>\n",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke>\n",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}tool_calls>"
        );
        let calls = parse_degraded_tool_calls(dsml).expect("fullwidth DSML should parse");
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| call["function"]["name"] == "bash"));
    }

    #[test]
    fn strip_dsml_wrapped_invoke_preserves_prose() {
        let dsml = concat!(
            "Web review complete.\n\n",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}tool_calls>",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke name=\"agent\">",
            "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter name=\"action\">get_result",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter>",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke>",
            "</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}tool_calls>",
        );

        assert_eq!(strip_degraded_tool_calls(dsml), "Web review complete.\n\n");
    }

    #[test]
    fn strip_dsml_wrapped_invoke_with_tag_whitespace() {
        let dsml = concat!(
            "Done.\n",
            "< ||DSML||tool_calls >",
            "< ||DSML||invoke name=\"agent\">",
            "< ||DSML||parameter name=\"action\">get_result",
            "</ ||DSML||parameter>",
            "</ ||DSML||invoke>",
            "</ ||DSML||tool_calls >",
        );

        let calls = parse_xml_tool_calls(dsml).unwrap();
        assert_eq!(calls[0]["function"]["name"], "agent");
        assert_eq!(strip_degraded_tool_calls(dsml), "Done.\n");
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
        // Even when the model discusses the XML format, a valid <invoke> block
        // is treated as a tool call — the structured format is unambiguous.
        // (Previously this was rejected by a ratio guard, but that caused real
        // tool calls with prose prefixes to leak to output.)
        let text = r#"The system uses XML tool calls like this:
<invoke name="read_file">
<parameter name="path">src/lib.rs</parameter>
</invoke>
This format is used when the model degrades under context pressure.
You can see the invoke block contains parameter elements."#;
        let calls = parse_xml_tool_calls(text);
        assert!(
            calls.is_some(),
            "valid invoke block should always be parsed"
        );
        assert_eq!(calls.unwrap()[0]["function"]["name"], "read_file");
    }

    #[test]
    fn rejects_single_invoke_in_explanation() {
        // Even in an explanatory context, a valid <invoke> block is parsed as a tool call.
        let text = r#"Here is an example of the invoke format:
<invoke name="bash">
<parameter name="command">echo hello</parameter>
</invoke>
As you can see, the name attribute specifies the tool."#;
        let calls = parse_xml_tool_calls(text);
        assert!(calls.is_some());
        assert_eq!(calls.unwrap()[0]["function"]["name"], "bash");
    }

    #[test]
    fn strip_degraded_calls_preserves_explanatory_trailing_prose() {
        let text = r#"Let me check.
<invoke name="read_file">
<parameter name="path">a.rs</parameter>
</invoke>
Done."#;
        assert!(parse_degraded_tool_calls(text).is_none());
        assert_eq!(strip_degraded_tool_calls(text), text);
    }

    #[test]
    fn strip_preserves_unparseable_fragments() {
        let text = "some text <invoke broken";
        let remaining = strip_degraded_tool_calls(text);
        assert_eq!(remaining, text);
    }

    #[test]
    fn real_world_multi_invoke_output() {
        // A provider-neutral multi-call response with prose-free invoke blocks.
        let xml = r#"<invoke name="read_file">
<parameter name="path">crates/runtime/src/tasks/task_learning.rs</parameter>
</invoke>
<invoke name="read_file">
<parameter name="path">crates/runtime/src/tasks/pattern.rs</parameter>
</invoke>
<invoke name="grep">
<parameter name="pattern">success_rate|failure|penalty|decay|expire|outdated|bad|quality</parameter>
<parameter name="path">crates/runtime/src/tasks</parameter>
</invoke>"#;
        let calls = parse_xml_tool_calls(xml).unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[1]["function"]["name"], "read_file");
        assert_eq!(calls[2]["function"]["name"], "grep");

        let args0: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args0["path"], "crates/runtime/src/tasks/task_learning.rs");

        let args2: serde_json::Map<String, Value> =
            serde_json::from_str(calls[2]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(
            args2["pattern"],
            "success_rate|failure|penalty|decay|expire|outdated|bad|quality"
        );

        let remaining = strip_degraded_tool_calls(xml);
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
        let calls = parse_degraded_tool_calls(text).unwrap();
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
        let calls = parse_degraded_tool_calls(text).unwrap();
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
        let calls = parse_degraded_tool_calls(text).unwrap();
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
        let calls = parse_degraded_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "bash");
    }

    #[test]
    fn tool_call_tag_no_closing_tag() {
        // Some models don't emit closing tag
        let text = "<tool_call>bash)(echo hi)";
        let calls = parse_degraded_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "bash");
    }

    #[test]
    fn tool_call_tag_returns_none_for_no_tags() {
        assert!(parse_degraded_tool_calls("just normal text").is_none());
        assert!(parse_degraded_tool_calls("").is_none());
    }

    #[test]
    fn tool_call_tag_rejects_prose_with_tag() {
        let text = "The model emitted <tool_call>bash)(echo hello)</tool_call> which is a known degradation pattern that we handle.";
        assert!(parse_degraded_tool_calls(text).is_none());
    }

    #[test]
    fn tool_call_tag_strip_removes_parsed() {
        let text = "Let me try.\n<tool_call>bash)(echo hi)</tool_call>\nDone.";
        assert!(parse_degraded_tool_calls(text).is_none());
        assert_eq!(strip_degraded_tool_calls(text), text);

        let call = "<tool_call>bash)(echo hi)</tool_call>";
        assert_eq!(parse_degraded_tool_calls(call).unwrap().len(), 1);
        assert!(strip_degraded_tool_calls(call).is_empty());
    }

    #[test]
    fn tool_call_real_session_pattern_bash() {
        // Exact pattern from session 3bc7fc43
        let text = r#"<tool_call>bash)(git log -1 --format="%H%n%an%n%ae%n%s%n%b" HEAD)"#;
        let calls = parse_degraded_tool_calls(text).unwrap();
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
        let calls = parse_degraded_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "mcp__git_log");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["per_page"], 1);
    }

    // ─── Unified Parser Tests ──────────────────────────────────────

    #[test]
    fn unified_keeps_mixed_fallback_calls_in_source_order() {
        let text = r#"<tool_call>bash)(echo first)</tool_call>
<invoke name="bash">
<parameter name="command">echo second</parameter>
</invoke>"#;
        let calls = parse_degraded_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0]["function"]["arguments"],
            r#"{"command":"echo first"}"#
        );
        assert_eq!(
            calls[1]["function"]["arguments"],
            r#"{"command":"echo second"}"#
        );
    }

    #[test]
    fn unified_deduplicates_the_same_call_across_fallback_formats() {
        let text = r#"<invoke name="bash">
<parameter name="command">echo same</parameter>
</invoke>
<tool_call>bash)(echo same)</tool_call>"#;
        let calls = parse_degraded_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
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

    // ─── Regression: prose-prefixed invoke leak (session 47ff190c) ──────────
    //
    // LLM outputs a sentence of prose before the <invoke> block.
    // With MAX_NON_XML_RATIO=0.20 the prose pushes non-xml ratio above the
    // threshold and parse_xml_tool_calls returns None — the XML leaks to output.

    #[test]
    fn prose_prefix_invoke_is_parsed_not_leaked() {
        // Exact pattern from the user-reported session: one sentence of prose
        // followed by a valid <invoke> block.
        let text = "1. Let me start by exploring the project structure and understanding the context better.\n\n\
<invoke name=\"list_dir\">\n\
<parameter name=\"path\">/workspace/astra</parameter>\n\
</invoke>";
        let calls = parse_degraded_tool_calls(text);
        assert!(
            calls.is_some(),
            "invoke block with prose prefix should be parsed, not leaked to output"
        );
        let calls = calls.unwrap();
        assert_eq!(calls[0]["function"]["name"], "list_dir");
    }

    #[test]
    fn prose_prefix_invoke_is_stripped_from_text() {
        let text = "1. Let me start by exploring the project structure and understanding the context better.\n\n\
<invoke name=\"list_dir\">\n\
<parameter name=\"path\">/workspace/astra</parameter>\n\
</invoke>";
        let stripped = strip_degraded_tool_calls(text);
        assert!(
            !stripped.contains("<invoke"),
            "invoke block must be stripped from output text, got: {stripped:?}"
        );
    }

    #[test]
    fn truncated_invoke_tail_is_stripped_from_tool_turn_text() {
        let text = "I will inspect the repository.\n\n\
<invoke name=\"bash\">\n\
<parameter name=\"command\">pwd</parameter>";
        let stripped = strip_degraded_tool_calls(text);
        assert_eq!(stripped, "I will inspect the repository.");
        assert!(!stripped.contains("<invoke"));
        assert!(!stripped.contains("<parameter"));
        assert!(!stripped.contains("pwd"));
    }

    #[test]
    fn inline_invoke_mentions_are_not_stripped_as_truncated_tool_blocks() {
        let text = "The literal <invoke name=\"bash\"> tag appears in this review.";
        assert_eq!(strip_degraded_tool_calls(text), text);
    }

    #[test]
    fn bare_invoke_with_trailing_prose_is_not_executed() {
        // A bare invoke followed by explanatory text is ambiguous: preserve
        // the response instead of executing a block that may only be an
        // example in the explanation.
        let text = "I'll help you with that. Let me check the directory first.\n\n\
<invoke name=\"bash\">\n\
<parameter name=\"command\">ls -la</parameter>\n\
</invoke>\n\nThen I'll analyze the results.";
        let calls = parse_degraded_tool_calls(text);
        assert!(calls.is_none());
    }

    #[test]
    fn nested_markup_inside_invoke_parameter_is_not_a_second_call() {
        let text = r#"<invoke name="bash">
<parameter name="command">printf '<tool_call>bash)(echo nested)</tool_call>'</parameter>
</invoke>"#;
        let calls = parse_degraded_tool_calls(text).expect("outer invoke should parse");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "bash");
        assert_eq!(strip_degraded_tool_calls(text), "");
    }

    #[test]
    fn adjacent_mixed_fallback_blocks_are_not_treated_as_nested() {
        let text = r#"<invoke name="bash"><parameter name="command">echo one</parameter></invoke>
<tool_call>bash)(echo two)</tool_call>"#;
        let calls = parse_degraded_tool_calls(text).expect("both adjacent blocks should parse");
        assert_eq!(calls.len(), 2);
        assert_eq!(strip_degraded_tool_calls(text), "");
    }

    #[test]
    fn cross_format_dedup_consumes_only_one_occurrence() {
        let text = r#"<invoke name="bash"><parameter name="command">echo same</parameter></invoke>
<tool_call>bash)(echo same)</tool_call>
<tool_call>bash)(echo same)</tool_call>"#;
        let calls = parse_degraded_tool_calls(text).expect("fallback blocks should parse");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn cross_format_dedup_canonicalizes_json_argument_key_order() {
        let text = r#"<invoke name="mcp">
<parameter name="z">1</parameter><parameter name="a">2</parameter>
</invoke>
<tool_call>mcp{"a":"2","z":"1"}</tool_call>"#;
        let calls = parse_degraded_tool_calls(text).expect("fallback blocks should parse");
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn rejected_tag_is_not_stripped_when_a_different_invoke_is_accepted() {
        let text = concat!(
            "< ||DSML||tool_calls >< ||DSML||invoke name=\"bash\">",
            "< ||DSML||parameter name=\"command\">echo ok</ ||DSML||parameter>",
            "</ ||DSML||invoke></ ||DSML||tool_calls>\n",
            "This explanation is intentionally long enough to make the adjacent ",
            "ambiguous tag untrusted: <tool_call>bash)(echo maybe)</tool_call>"
        );
        assert_eq!(parse_degraded_tool_calls(text).unwrap().len(), 1);
        let stripped = strip_degraded_tool_calls(text);
        assert!(stripped.contains("<tool_call>"));
    }

    #[test]
    fn strip_residual_fragments_removes_parameter_tags() {
        let input = "所有任务已完成。更新一下 task board 状态：  <parameter=action> update  <parameter=new_status> completed";
        let result = strip_residual_xml_fragments(input);
        assert!(!result.contains("<parameter="));
        assert!(!result.contains("<function="));
        assert!(result.contains("所有任务已完成"));
        assert!(!result.contains("update"));
        assert!(!result.contains("completed"));
    }

    #[test]
    fn strip_residual_fragments_removes_function_tags() {
        let input = "text\n<function=task> <parameter=action> update  <parameter=new_status> completed  <parameter=task_id> task-5\nmore";
        let result = strip_residual_xml_fragments(input);
        assert!(!result.contains("<parameter="));
        assert!(!result.contains("<function="));
        assert!(result.contains("text"));
        assert!(result.contains("more"));
        assert!(!result.contains("update"));
        assert!(!result.contains("task-5"));
    }

    #[test]
    fn strip_residual_fragments_preserves_normal_text() {
        let input = "normal text without any fragments at all";
        let result = strip_residual_xml_fragments(input);
        assert_eq!(result, input);
    }

    #[test]
    fn strip_residual_fragments_handles_user_transcript() {
        let input = "所有任务已完成。更新一下 stale 的 task board 状态：  <parameter=action> update  <parameter=new_status> completed\n<parameter=task_id> task-3\n\n<function=task> <parameter=action> update  <parameter=new_status> completed  <parameter=task_id> task-4\n\n<function=task> <parameter=action> update  <parameter=new_status> completed  <parameter=task_id> task-5";
        let result = strip_residual_xml_fragments(input);
        assert!(!result.contains("<parameter="));
        assert!(!result.contains("<function="));
        assert!(result.contains("所有任务已完成"));
        assert!(!result.contains("task-3"));
        assert!(!result.contains("task-4"));
        assert!(!result.contains("task-5"));
    }
}
