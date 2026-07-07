//! Phase L (XML fallback half) — LLM hallucination guardrails for the
//! XML tool-call parser that recovers tool calls from degraded / plain
//! text assistant output.
//!
//! See also `phase_l_composition_validation.rs` in the `astra-skills`
//! crate for the JSON-Schema side of Phase L.

use astra_turn_core::xml_tool_call_fallback::{parse_xml_tool_calls, strip_parsed_invocations};

#[test]
fn phase_l_xml_prose_heavy_invoke_is_parsed() {
    // Previously this was rejected by a ratio guard (MAX_NON_XML_RATIO).
    // The guard caused real tool calls with prose prefixes to leak to output
    // (regression from session 47ff190c).
    //
    // The <invoke name="..."><parameter>...</parameter></invoke> format is
    // unambiguous — it only appears as actual tool calls, never in explanatory
    // prose. So we always parse it regardless of surrounding text.
    let prose = "Here is a discussion about tool-call XML. ".repeat(20);
    let text =
        format!("{prose}<invoke name=\"bash\"><parameter name=\"cmd\">ls</parameter></invoke>");
    let result = parse_xml_tool_calls(&text);
    assert!(
        result.is_some(),
        "invoke block must always be parsed regardless of surrounding prose"
    );
    assert_eq!(result.unwrap()[0]["function"]["name"], "bash");
}

#[test]
fn phase_l_xml_no_invoke_tag_returns_none() {
    assert!(parse_xml_tool_calls("plain assistant text with no XML").is_none());
    assert!(parse_xml_tool_calls("").is_none());
}

#[test]
fn phase_l_xml_unclosed_invoke_does_not_hang() {
    let text = "<invoke name=\"bash\"><parameter name=\"cmd\">ls";
    let result = parse_xml_tool_calls(text);
    assert!(result.is_none());
}

#[test]
fn phase_l_xml_multiple_invokes_parsed() {
    let text = "<invoke name=\"read_file\"><parameter name=\"path\">/a.rs</parameter></invoke>\
                <invoke name=\"read_file\"><parameter name=\"path\">/b.rs</parameter></invoke>";
    let calls = parse_xml_tool_calls(text).expect("should parse");
    assert_eq!(calls.len(), 2, "both invocations must parse");
}

#[test]
fn phase_l_xml_strip_parsed_invocations_leaves_prose() {
    let text = "Before.<invoke name=\"ls\"/>After.";
    let remaining = strip_parsed_invocations(text);
    assert!(remaining.contains("Before."));
    assert!(remaining.contains("After."));
    assert!(!remaining.contains("<invoke"));
}

#[test]
fn phase_l_xml_strip_preserves_unparseable_fragments() {
    let text = "Opening<invoke name=\"bad and no end tag";
    let remaining = strip_parsed_invocations(text);
    assert!(remaining.contains("<invoke"));
}
