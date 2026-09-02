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

use regex::Regex;
use serde_json::{Value, json};
use std::ops::Range;
use std::sync::OnceLock;
use uuid::Uuid;

/// Maximum ratio of non-XML text to total text before we refuse to treat the
/// content as degraded `<tool_call>` blocks.  0.20 = if more than 20% of the
/// content is plain prose surrounding the tags, it's probably a normal
/// response that happens to mention the format.
const MAX_NON_XML_RATIO: f64 = 0.20;
const DSML_FULLWIDTH_PREFIX: &str = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";
const DSML_ASCII_PREFIX: &str = "||DSML||";

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
    let normalized = normalize_dsml_tool_call_markup(text);
    let text = normalized.as_str();
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

    if calls.is_empty() {
        return None;
    }

    // No false-positive ratio guard here: `<invoke name="..."><parameter>...</parameter></invoke>`
    // is a highly specific structured format that only appears as actual tool calls, never in
    // explanatory prose. (The <tool_call> parser keeps its own guard for its more ambiguous format.)

    Some(calls)
}

/// Strip successfully-parsed `<invoke>` blocks from text, returning the
/// remaining content (trimmed).  Unparseable fragments are kept.
pub fn strip_parsed_invocations(text: &str) -> String {
    if let Some(stripped) = strip_parsed_dsml_tool_call_blocks(text) {
        return stripped;
    }
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

fn normalize_dsml_tool_call_markup(text: &str) -> String {
    let mut normalized = text.to_string();
    for tag in ["tool_calls", "invoke", "parameter"] {
        let open = dsml_tag_regex(tag, false);
        let close = dsml_tag_regex(tag, true);
        normalized = open
            .replace_all(&normalized, format!("<{tag}").as_str())
            .into_owned();
        normalized = close
            .replace_all(&normalized, format!("</{tag}").as_str())
            .into_owned();
    }
    normalized
}

fn strip_parsed_dsml_tool_call_blocks(text: &str) -> Option<String> {
    let mut result = text.to_string();
    let mut changed = false;
    let open = dsml_tool_calls_open_regex();
    let close = dsml_tool_calls_close_regex();

    let mut search_from = 0;

    while let Some(open_match) = open.find(&result[search_from..]) {
        let abs_start = search_from + open_match.start();
        let body_start = search_from + open_match.end();
        let Some(close_match) = close.find(&result[body_start..]) else {
            search_from = body_start;
            continue;
        };
        let block_end = body_start + close_match.end();
        let block = &result[abs_start..block_end];
        if parse_xml_tool_calls(block).is_some() {
            result.replace_range(abs_start..block_end, "");
            changed = true;
        } else {
            search_from = block_end;
        }
    }

    changed.then(|| result.trim().to_string())
}

fn dsml_tag_regex(tag: &str, closing: bool) -> Regex {
    let slash = if closing { r"/\s*" } else { "" };
    Regex::new(&format!(
        r"(?i)<\s*{slash}(?:{}|{})\s*{}",
        regex::escape(DSML_FULLWIDTH_PREFIX),
        regex::escape(DSML_ASCII_PREFIX),
        regex::escape(tag)
    ))
    .expect("DSML tag regex should compile")
}

fn dsml_tool_calls_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?i)<\s*(?:{}|{})\s*tool_calls\s*>",
            regex::escape(DSML_FULLWIDTH_PREFIX),
            regex::escape(DSML_ASCII_PREFIX)
        ))
        .expect("DSML tool_calls open regex should compile")
    })
}

fn dsml_tool_calls_close_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?i)<\s*/\s*(?:{}|{})\s*tool_calls\s*>",
            regex::escape(DSML_FULLWIDTH_PREFIX),
            regex::escape(DSML_ASCII_PREFIX)
        ))
        .expect("DSML tool_calls close regex should compile")
    })
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

    // False-positive guard: reject if prose dominates the text.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// A DSML tool_calls wrapper is stronger evidence than a bare invoke tag.
    wrapped: bool,
}

#[derive(Debug, Default)]
struct ParsedDegradedResponse {
    calls: Vec<Value>,
    /// Exact source ranges accepted as tool-call syntax. The strip path uses
    /// these same ranges, so it cannot remove a candidate rejected by the
    /// parse/ambiguity policy.
    strip_ranges: Vec<Range<usize>>,
}

fn next_invoke_start(text: &str, from: usize) -> Option<usize> {
    let plain = text[from..].find("<invoke").map(|offset| from + offset);
    let dsml = dsml_tag_regex("invoke", false)
        .find(&text[from..])
        .map(|matched| from + matched.start());
    [plain, dsml].into_iter().flatten().min()
}

fn invoke_block_end(text: &str, start: usize) -> usize {
    let tail = &text[start..];
    let plain_close = tail
        .find("</invoke>")
        .map(|offset| start + offset + "</invoke>".len());
    let dsml_close = dsml_tag_regex("invoke", true).find(tail).map(|matched| {
        let end = start + matched.end();
        end + text[end..].find('>').map(|offset| offset + 1).unwrap_or(0)
    });
    let self_close = tail.find("/>").map(|offset| start + offset + 2);
    [plain_close, dsml_close, self_close]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(text.len())
}

fn dsml_tool_calls_ranges(text: &str) -> Vec<Range<usize>> {
    let open = dsml_tool_calls_open_regex();
    let close = dsml_tool_calls_close_regex();
    let mut ranges = Vec::new();
    let mut search_from = 0;
    while let Some(open_match) = open.find(&text[search_from..]) {
        let start = search_from + open_match.start();
        let body_start = search_from + open_match.end();
        let Some(close_match) = close.find(&text[body_start..]) else {
            break;
        };
        let end = body_start + close_match.end();
        ranges.push(start..end);
        search_from = end;
    }
    ranges
}

fn span_is_inside(span: &Range<usize>, outer: &Range<usize>) -> bool {
    span.start >= outer.start && span.end <= outer.end
}

fn parse_invokes_with_positions(text: &str) -> Vec<ParsedDegradedCall> {
    let mut calls = Vec::new();
    let wrappers = dsml_tool_calls_ranges(text);
    let mut search_from = 0;
    while let Some(start) = next_invoke_start(text, search_from) {
        let end = invoke_block_end(text, start);
        let block = normalize_dsml_tool_call_markup(&text[start..end]);
        if let Some(call) = parse_xml_tool_calls(&block).and_then(|mut parsed| parsed.pop()) {
            let span = start..end;
            calls.push(ParsedDegradedCall {
                wrapped: wrappers
                    .iter()
                    .any(|wrapper| span_is_inside(&span, wrapper)),
                span,
                format: DegradedCallFormat::Invoke,
                call,
            });
        }
        // Always move forward, including malformed blocks, so a bad block
        // cannot make the fallback parser spin forever.
        search_from = end.max(start.saturating_add(1));
    }
    calls
}

fn parse_tool_call_tags_with_positions(text: &str) -> Vec<ParsedDegradedCall> {
    if !text.contains("<tool_call>") {
        return Vec::new();
    }

    let mut calls = Vec::new();
    let mut search_from = 0;
    while let Some(start_offset) = text[search_from..].find("<tool_call>") {
        let start = search_from + start_offset;
        let content_start = start + "<tool_call>".len();
        let end = text[content_start..]
            .find("</tool_call>")
            .map(|offset| content_start + offset + "</tool_call>".len())
            .or_else(|| {
                text[content_start..]
                    .find("<tool_call>")
                    .map(|offset| content_start + offset)
            })
            .unwrap_or(text.len());
        let inner = text[content_start..end]
            .trim_end_matches("</tool_call>")
            .replace('\0', "");
        if let Some(call) = parse_single_tool_call_tag(inner.trim()) {
            calls.push(ParsedDegradedCall {
                span: start..end,
                format: DegradedCallFormat::ToolCallTag,
                call,
                wrapped: false,
            });
        }
        search_from = end.max(start.saturating_add(1));
    }

    calls
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
        .position(|call| call.format == DegradedCallFormat::Invoke && !call.wrapped)
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
    let mut candidates = parse_invokes_with_positions(text);
    candidates.extend(parse_tool_call_tags_with_positions(text));
    let candidates = select_non_overlapping(candidates);
    let bare_suffix = bare_invokes_form_terminal_suffix(text, &candidates);
    let accepted_invokes: Vec<ParsedDegradedCall> = candidates
        .iter()
        .filter(|invoke| {
            invoke.format == DegradedCallFormat::Invoke && (invoke.wrapped || bare_suffix)
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
        .filter(|tag| {
            !accepted_invoke_ranges
                .iter()
                .any(|invoke| tag.span.start < invoke.end && invoke.start < tag.span.end)
        })
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
    let mut seen_identities = Vec::<(DegradedCallFormat, (String, String), usize)>::new();
    for entry in &parsed {
        let identity = degraded_call_identity(&entry.call);
        let mut duplicate = false;
        if let Some(identity) = identity.as_ref() {
            let current_count = seen_identities
                .iter()
                .find(|(format, seen, _)| *format == entry.format && *seen == *identity)
                .map(|(_, _, count)| *count)
                .unwrap_or(0);
            let other_available = seen_identities.iter().any(|(format, seen, count)| {
                *format != entry.format && *seen == *identity && *count > 0
            });
            duplicate = current_count == 0 && other_available;
            if let Some((_, _, count)) = seen_identities
                .iter_mut()
                .find(|(format, seen, _)| *format == entry.format && *seen == *identity)
            {
                *count += 1;
            } else {
                seen_identities.push((entry.format, identity.clone(), 1));
            }
        }
        if !duplicate {
            calls.push(entry.call.clone());
        }
    }

    let mut strip_ranges = accepted_invoke_ranges;
    let accepted_invoke_ranges_ref = strip_ranges.clone();
    // Strip a DSML wrapper as one unit so its wrapper tags do not leak into
    // the assistant transcript. Bare invokes retain their exact block range.
    strip_ranges.extend(dsml_tool_calls_ranges(text).into_iter().filter(|wrapper| {
        accepted_invoke_ranges_ref
            .iter()
            .any(|range| span_is_inside(range, wrapper))
    }));
    strip_ranges.extend(
        parsed
            .iter()
            .filter(|entry| entry.format == DegradedCallFormat::ToolCallTag)
            .map(|entry| entry.span.clone()),
    );

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
    let after_truncated_tail = strip_truncated_degraded_tool_call_tail(&after_accepted);
    strip_residual_xml_fragments(&after_truncated_tail)
}

fn strip_truncated_degraded_tool_call_tail(text: &str) -> String {
    let Some(start) = truncated_degraded_tool_call_tail_start(text) else {
        return text.to_string();
    };
    text[..start].trim_end().to_string()
}

fn truncated_degraded_tool_call_tail_start(text: &str) -> Option<usize> {
    ["<invoke", "<tool_call"]
        .into_iter()
        .filter_map(|marker| text.rfind(marker).map(|pos| (marker, pos)))
        .filter(|(marker, pos)| {
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
fn strip_residual_xml_fragments(text: &str) -> String {
    use regex::Regex;

    if !text.contains("<parameter=") && !text.contains("<function=") {
        return text.to_string();
    }

    // `<parameter=KEY>` or `<parameter=KEY>VALUE` — strip tag and the
    // immediately-following word (the "value") when present on the same line.
    static RE_PARAM: OnceLock<Regex> = OnceLock::new();
    let re_param = RE_PARAM.get_or_init(|| Regex::new(r"<parameter=[^>]+>\s*\S*").unwrap());

    // `<function=NAME>` — self-closing fragment, no value.
    static RE_FUNC: OnceLock<Regex> = OnceLock::new();
    let re_func = RE_FUNC.get_or_init(|| Regex::new(r"<function=[^>]+>").unwrap());

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

    result.trim().to_string()
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

        assert_eq!(strip_parsed_invocations(dsml), "Web review complete.");
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
        assert_eq!(strip_parsed_invocations(dsml), "Done.");
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
