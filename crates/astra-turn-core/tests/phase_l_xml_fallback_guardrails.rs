//! Phase L (XML fallback half) — LLM hallucination guardrails for the
//! XML tool-call parser that recovers tool calls from degraded / plain
//! text assistant output.
//!
//! See also `phase_l_composition_validation.rs` in the `astra-skills`
//! crate for the JSON-Schema side of Phase L.

use astra_turn_core::xml_tool_call_fallback::{
    parse_degraded_tool_calls, strip_degraded_tool_calls,
};

#[test]
fn phase_l_xml_prose_heavy_invoke_is_parsed() {
    // A prose prefix is valid only when the invocation is the terminal suffix.
    let prose = "Here is a discussion about tool-call XML. ".repeat(20);
    let text =
        format!("{prose}<invoke name=\"bash\"><parameter name=\"cmd\">ls</parameter></invoke>");
    let result = parse_degraded_tool_calls(&text);
    assert!(
        result.is_some(),
        "terminal invocation must parse even with a prose prefix"
    );
    assert_eq!(result.unwrap()[0]["function"]["name"], "bash");
}

#[test]
fn phase_l_xml_no_invoke_tag_returns_none() {
    assert!(parse_degraded_tool_calls("plain assistant text with no XML").is_none());
    assert!(parse_degraded_tool_calls("").is_none());
}

#[test]
fn phase_l_xml_unclosed_invoke_does_not_hang() {
    let text = "<invoke name=\"bash\"><parameter name=\"cmd\">ls";
    let result = parse_degraded_tool_calls(text);
    assert!(result.is_none());
}

#[test]
fn phase_l_xml_multiple_invokes_parsed() {
    let text = "<invoke name=\"read_file\"><parameter name=\"path\">/a.rs</parameter></invoke>\
                <invoke name=\"read_file\"><parameter name=\"path\">/b.rs</parameter></invoke>";
    let calls = parse_degraded_tool_calls(text).expect("should parse");
    assert_eq!(calls.len(), 2, "both invocations must parse");
}

#[test]
fn phase_l_xml_strip_terminal_invocation_leaves_prefix() {
    let text = "Before.<invoke name=\"ls\"/>";
    let remaining = strip_degraded_tool_calls(text);
    assert!(remaining.contains("Before."));
    assert!(!remaining.contains("<invoke"));
}

#[test]
fn phase_l_xml_strip_preserves_inline_discussion() {
    let text = "An inline example mentions <invoke broken syntax.";
    let remaining = strip_degraded_tool_calls(text);
    assert!(remaining.contains("<invoke"));
}
