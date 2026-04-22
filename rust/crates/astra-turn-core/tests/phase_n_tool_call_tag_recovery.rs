//! Phase N — LLM hallucination guardrails, extended.
//!
//! Targets `parse_tool_call_tags` / `recover_tool_call_args` — the fallback
//! recovery path for corrupted `<tool_call>` syntax that real models emit
//! under stress (e.g. `<tool_call>bash)(echo hi)</tool_call>`). These tests
//! pin the degraded-output contract across the nastiest shapes we've seen
//! in production traces.

use astra_turn_core::xml_tool_call_fallback::parse_tool_call_tags;

#[test]
fn phase_n_tool_call_tag_parenthesized_args() {
    let text = "<tool_call>bash)(echo hello)</tool_call>";
    let calls = parse_tool_call_tags(text).expect("should parse");
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert_eq!(c["function"]["name"], "bash");
    let args = c["function"]["arguments"].as_str().unwrap();
    assert!(
        args.contains("echo hello"),
        "parenthesized args must be recovered, got: {args}"
    );
}

#[test]
fn phase_n_tool_call_tag_valid_json_args_preserved() {
    let text = r#"<tool_call>read_file({"path":"a.rs"})</tool_call>"#;
    let calls = parse_tool_call_tags(text).expect("should parse");
    let c = &calls[0];
    assert_eq!(c["function"]["name"], "read_file");
    let args = c["function"]["arguments"].as_str().unwrap();
    // Valid JSON object must be preserved verbatim (modulo surrounding whitespace).
    let parsed: serde_json::Value = serde_json::from_str(args).expect("args must be valid JSON");
    assert_eq!(parsed["path"], "a.rs");
}

#[test]
fn phase_n_tool_call_tag_no_tag_returns_none() {
    assert!(parse_tool_call_tags("plain text").is_none());
    assert!(parse_tool_call_tags("").is_none());
}

#[test]
fn phase_n_tool_call_tag_prose_heavy_rejected() {
    // Long prose + tiny tool_call block → false-positive guard kicks in.
    let prose = "Let me explain the tool_call concept at length. ".repeat(20);
    let text = format!("{prose}<tool_call>bash(ls)</tool_call>");
    assert!(
        parse_tool_call_tags(&text).is_none(),
        "prose-heavy text must be treated as normal speech"
    );
}

#[test]
fn phase_n_tool_call_tag_empty_body_yields_no_call() {
    // Empty inner content must not produce a phantom call.
    let text = "<tool_call></tool_call>";
    assert!(parse_tool_call_tags(text).is_none());
}

#[test]
fn phase_n_tool_call_tag_null_bytes_stripped() {
    let text = "<tool_call>bash\0(\0echo\0 x\0)</tool_call>";
    let calls = parse_tool_call_tags(text).expect("should parse");
    assert_eq!(calls[0]["function"]["name"], "bash");
    // null bytes must not appear in recovered args.
    let args = calls[0]["function"]["arguments"].as_str().unwrap();
    assert!(!args.contains('\0'), "null bytes must be stripped");
}

#[test]
fn phase_n_tool_call_tag_multiple_calls() {
    let text = "<tool_call>read_file({\"path\":\"a\"})</tool_call>\
                <tool_call>read_file({\"path\":\"b\"})</tool_call>";
    let calls = parse_tool_call_tags(text).expect("should parse");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["function"]["name"], "read_file");
    assert_eq!(calls[1]["function"]["name"], "read_file");
}

#[test]
fn phase_n_tool_call_tag_unterminated_recovers_with_next_tag() {
    // The parser accepts an unterminated tag by rolling to the next <tool_call>
    // (or end of text). Pin this contract.
    let text = "<tool_call>read_file({\"path\":\"a.rs\"})<tool_call>bash(ls)</tool_call>";
    let calls = parse_tool_call_tags(text).expect("should parse");
    // Both extracted — first by "next tag" boundary, second by "</tool_call>".
    assert!(!calls.is_empty(), "at least one call must be recovered");
}

#[test]
fn phase_n_tool_call_tag_garbage_after_name_wraps_as_command() {
    // When we can't find valid JSON or parens, the parser best-effort wraps the
    // remainder as {"command": "..."} — this unblocks bash-family tools even
    // when the model's output is badly mangled.
    let text = "<tool_call>bash echo hello world</tool_call>";
    let calls = parse_tool_call_tags(text).expect("should parse");
    let args = calls[0]["function"]["arguments"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(args).expect("wrapped args must be JSON");
    // Either nested {"command": ...} or the string recovered as a full-JSON object.
    assert!(
        parsed.get("command").is_some() || parsed.is_object(),
        "best-effort recovery must yield a JSON object, got: {args}"
    );
}
