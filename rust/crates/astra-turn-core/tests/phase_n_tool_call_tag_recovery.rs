//! Phase N (proptest-rewrite) — invariant testing for `parse_tool_call_tags`.
//!
//! Rationale: the v1 of this file pinned a handful of hand-picked corrupted
//! inputs with loose assertions (`!calls.is_empty()`, `.is_object()`). A
//! mutant that corrupts the recovered JSON would still pass.
//!
//! This rewrite asserts **design invariants** that must hold for ANY input.
//! A property test that finds a single counterexample surfaces a real bug.
//!
//! Invariants asserted for every input string:
//!   I1. Never panics.
//!   I2. If Some(calls) is returned, every `function.arguments` string is
//!       valid JSON. (Contract: executor always gets parseable args.)
//!   I3. Every `function.name` is non-empty and contains only `[A-Za-z0-9_-]`.
//!   I4. Null bytes never leak into names or arguments (the parser strips
//!       them up front).
//!   I5. Call count is bounded by the number of `<tool_call>` occurrences
//!       in the input. (Guards against duplication bugs.)
//!   I6. Every returned call has a stable JSON shape: `{id, type:"function",
//!       function:{name, arguments}}`.
//!
//! Anchor cases (not property-generated) still cover specific real-world
//! corrupted shapes: parenthesized args, JSON args, prose-heavy guard.

use astra_turn_core::xml_tool_call_fallback::parse_tool_call_tags;
use proptest::prelude::*;
use serde_json::Value;

// ── Invariant check helper ──────────────────────────────────────────────────

fn assert_invariants(input: &str, result: Option<Vec<Value>>) {
    let Some(calls) = result else { return };

    let tag_count = input.matches("<tool_call>").count();
    // I5: cannot synthesize more calls than there are opening tags.
    assert!(
        calls.len() <= tag_count,
        "I5 violated: {} calls from {} tags\ninput={:?}",
        calls.len(),
        tag_count,
        input
    );

    for (idx, call) in calls.iter().enumerate() {
        // I6: JSON shape.
        let obj = call.as_object().unwrap_or_else(|| {
            panic!("I6: call {idx} not an object: {call:?}");
        });
        assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("function"));
        assert!(obj.contains_key("id"), "I6: call {idx} missing id");
        let function = obj
            .get("function")
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| panic!("I6: call {idx} has no function object"));

        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("I6: call {idx} has no name string"));
        let args = function
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("I6: call {idx} arguments not a string"));

        // I3: name well-formed.
        assert!(!name.is_empty(), "I3: empty name in call {idx}");
        assert!(
            name.chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-'),
            "I3: bad name chars {:?} in call {idx}\ninput={:?}",
            name,
            input
        );

        // I4: no null bytes in name / args.
        assert!(!name.contains('\0'), "I4: null byte in name");
        assert!(
            !args.contains('\0'),
            "I4: null byte in args\ninput={input:?}"
        );

        // I2: arguments must parse as JSON.
        let parsed: Result<Value, _> = serde_json::from_str(args);
        assert!(
            parsed.is_ok(),
            "I2 violated: args {:?} is not valid JSON\ninput={:?}\nerror={:?}",
            args,
            input,
            parsed.err()
        );
    }
}

// ── Property: never panics + invariants for arbitrary text ──────────────────

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        max_shrink_iters: 4096,
        .. ProptestConfig::default()
    })]

    #[test]
    fn phase_n_no_panic_on_arbitrary_text(s in ".*") {
        // I1 (never panics) + I2–I6 for arbitrary unicode input.
        let r = parse_tool_call_tags(&s);
        assert_invariants(&s, r);
    }

    // Generator: a realistic "tool_call with noise" input space.
    // Names: 1–8 chars from a word-ish alphabet.
    // Bodies: arbitrary bytes (exercises recovery paths).
    // We interleave 0–4 tool_call blocks and add surrounding noise.
    #[test]
    fn phase_n_structured_tool_call_inputs_obey_invariants(
        blocks in proptest::collection::vec(
            (
                "[A-Za-z_][A-Za-z0-9_]{0,7}",     // name
                "[^<]{0,40}"                     // body (no '<' to keep tag bounds clean)
            ),
            0..5
        ),
        prefix in "[^<]{0,30}",
        suffix in "[^<]{0,30}",
        close_every_other in any::<bool>(),
    ) {
        let mut s = String::new();
        s.push_str(&prefix);
        for (i, (name, body)) in blocks.iter().enumerate() {
            s.push_str("<tool_call>");
            s.push_str(name);
            s.push_str(body);
            // Sometimes omit the closing tag to exercise the unterminated path.
            if close_every_other || i % 2 == 0 {
                s.push_str("</tool_call>");
            }
        }
        s.push_str(&suffix);

        let r = parse_tool_call_tags(&s);
        assert_invariants(&s, r);
    }

    // Property: null bytes injected anywhere must never leak out.
    #[test]
    fn phase_n_null_bytes_never_leak(
        name in "[A-Za-z_][A-Za-z0-9_]{0,7}",
        noise_prefix in "[^<\0]{0,10}",
        noise_suffix in "[^<\0]{0,10}",
    ) {
        let s = format!(
            "<tool_call>\0{}\0{}\0(\0echo\0 hi\0)\0{}\0</tool_call>",
            noise_prefix, name, noise_suffix
        );
        let r = parse_tool_call_tags(&s);
        assert_invariants(&s, r);
    }
}

// ── Anchor cases: real-world corrupted shapes with *strong* assertions ──────

#[test]
fn phase_n_anchor_parenthesized_args_yield_command_json() {
    let text = "<tool_call>bash)(echo hello)</tool_call>";
    let calls = parse_tool_call_tags(text).expect("should parse");
    assert_eq!(calls.len(), 1);
    let args = calls[0]["function"]["arguments"].as_str().unwrap();
    // Strong assertion: the recovered args are JSON AND wrap echo hello as a
    // command field OR are a direct JSON object containing it.
    let v: Value = serde_json::from_str(args).expect("args must be JSON");
    let cmd = v.get("command").and_then(|x| x.as_str()).unwrap_or("");
    assert_eq!(cmd, "echo hello", "exact command recovery, got args={args}");
}

#[test]
fn phase_n_anchor_valid_json_args_preserved_exactly() {
    let text = r#"<tool_call>read_file({"path":"a.rs"})</tool_call>"#;
    let calls = parse_tool_call_tags(text).expect("should parse");
    let args = calls[0]["function"]["arguments"].as_str().unwrap();
    let v: Value = serde_json::from_str(args).expect("valid JSON");
    assert_eq!(v["path"], "a.rs");
    assert_eq!(v.as_object().unwrap().len(), 1, "no extra fields injected");
}

#[test]
fn phase_n_anchor_prose_heavy_rejected_to_avoid_false_positives() {
    let prose = "Let me explain the tool_call concept at length. ".repeat(20);
    let text = format!("{prose}<tool_call>bash(ls)</tool_call>");
    assert!(
        parse_tool_call_tags(&text).is_none(),
        "prose-heavy text must be rejected"
    );
}

#[test]
fn phase_n_anchor_empty_body_yields_none() {
    assert!(parse_tool_call_tags("<tool_call></tool_call>").is_none());
}

#[test]
fn phase_n_anchor_no_tag_returns_none() {
    assert!(parse_tool_call_tags("plain text no tags").is_none());
    assert!(parse_tool_call_tags("").is_none());
}

// ── Regression anchor: bug caught by proptest on first run ──────────────────
//
// Before the fix, `recover_tool_call_args` wrapped args as
//   format!("{{\"command\":\"{escaped}\"}}")
// escaping only `\` and `"`. Control characters (0x01–0x1F) other than the
// stripped NULs were emitted verbatim, producing strings that are NOT valid
// JSON per RFC 8259 (§7 — control chars inside strings must be escaped).
// Downstream executors call `serde_json::from_str` on `arguments` and would
// reject the tool call.
//
// Fix: use `serde_json::json!` to build and serialize the command wrapper,
// which escapes all control characters correctly.
#[test]
fn phase_n_regression_control_chars_in_recovered_args_are_valid_json() {
    // Real-world stand-in: LLM streams a byte like 0x01 (SOH) in the middle
    // of a corrupted tool-call body.
    let text = "<tool_call>bash echo \u{0001}hello\u{0002}world</tool_call>";
    let calls = parse_tool_call_tags(text).expect("should parse");
    let args = calls[0]["function"]["arguments"].as_str().unwrap();
    let v: Value = serde_json::from_str(args).expect("MUST be valid JSON even with control chars");
    assert!(v.get("command").and_then(|x| x.as_str()).is_some());
}
