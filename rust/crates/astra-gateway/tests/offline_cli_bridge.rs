//! Offline e2e tests using recorded fixtures — NO live LLM required.
//!
//! These tests parse real `astra chat --json` and `claude --output-format json`
//! output that was recorded once and saved as fixtures. They prove the gateway
//! can correctly parse real CLI output without making any API calls.
//!
//! To update fixtures: run the live e2e tests and copy stdout to fixtures/.

use astra_gateway::cli_bridge::CliProfile;

fn load_fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path} not found: {e}"))
}

// ─── Astra fixtures ─────────────────────────────────────────────────────────

#[test]
fn offline_astra_hello_parse() {
    let json = load_fixture("astra_hello.json");
    let profile = CliProfile::default(); // Astra
    let result = profile.parse_output(&json, 0);

    assert!(result.session_id.is_some(), "session_id present");
    assert_eq!(result.trace_id.as_deref(), Some("fixture-trace-hello"));
    assert_eq!(result.request_id.as_deref(), Some("fixture-request-hello"));
    assert!(result.run_id.is_some(), "run_id present");
    assert!(result.success, "success true");
    assert!(result.error_kind.is_none(), "error_kind null on success");
    assert!(result.text.is_some(), "text present");
    let text = result.text.unwrap();
    assert!(!text.is_empty(), "text non-empty");
    assert_eq!(result.exit_code, 0);
    assert!(result.tokens_prompt.unwrap_or(0) > 0, "prompt tokens > 0");
}

#[test]
fn offline_astra_tool_parse() {
    let json = load_fixture("astra_tool.json");
    let profile = CliProfile::default();
    let result = profile.parse_output(&json, 0);

    assert!(result.session_id.is_some());
    assert!(
        result.tool_calls_count.unwrap_or(0) > 0,
        "should have tool calls"
    );
    assert_eq!(result.trace_id.as_deref(), Some("fixture-trace-tool"));
    assert_eq!(result.request_id.as_deref(), Some("fixture-request-tool"));
    assert_eq!(result.tools_used, vec!["bash", "str_replace", "read_file"]);
    assert!(result.text.is_some());
}

#[test]
fn offline_astra_session_id_stable() {
    let json = load_fixture("astra_hello.json");
    let profile = CliProfile::default();
    let r1 = profile.parse_output(&json, 0);
    let r2 = profile.parse_output(&json, 0);
    assert_eq!(
        r1.session_id, r2.session_id,
        "same fixture = same session_id"
    );
}

// ─── Claude fixtures ────────────────────────────────────────────────────────

#[test]
fn offline_claude_hello_parse() {
    let json = load_fixture("claude_hello.json");
    let profile = CliProfile::Claude {
        bin: "claude".into(),
        model: None,
        stream_json: false,
        extra_args: vec![],
    };
    let result = profile.parse_output(&json, 0);

    assert!(result.session_id.is_some(), "session_id");
    assert!(result.text.is_some(), "text (from 'result' field)");
    let text = result.text.unwrap();
    assert!(!text.is_empty());
    assert!(
        result.tokens_completion.unwrap_or(0) > 0,
        "output tokens > 0"
    );
    println!(
        "claude fixture: text={text}, tokens={:?}",
        result.tokens_completion
    );
}

#[test]
fn offline_claude_session_id_format() {
    let json = load_fixture("claude_hello.json");
    let profile = CliProfile::Claude {
        bin: "claude".into(),
        model: None,
        stream_json: false,
        extra_args: vec![],
    };
    let result = profile.parse_output(&json, 0);
    let sid = result.session_id.unwrap();
    // Claude session IDs are UUIDs
    assert!(sid.len() > 20, "should be a UUID-like string: {sid}");
}

// ─── Cross-CLI comparison ───────────────────────────────────────────────────

#[test]
fn offline_both_clis_return_session_id() {
    let astra = {
        let json = load_fixture("astra_hello.json");
        CliProfile::default().parse_output(&json, 0)
    };
    let claude = {
        let json = load_fixture("claude_hello.json");
        CliProfile::Claude {
            bin: "claude".into(),
            model: None,
            stream_json: false,
            extra_args: vec![],
        }
        .parse_output(&json, 0)
    };

    assert!(astra.session_id.is_some(), "astra has session_id");
    assert!(claude.session_id.is_some(), "claude has session_id");
    // Different CLIs, different session formats
    assert_ne!(astra.session_id, claude.session_id);
}

#[test]
fn offline_both_clis_return_text() {
    let astra = {
        let json = load_fixture("astra_hello.json");
        CliProfile::default().parse_output(&json, 0)
    };
    let claude = {
        let json = load_fixture("claude_hello.json");
        CliProfile::Claude {
            bin: "claude".into(),
            model: None,
            stream_json: false,
            extra_args: vec![],
        }
        .parse_output(&json, 0)
    };

    assert!(astra.text.is_some() && !astra.text.as_ref().unwrap().is_empty());
    assert!(claude.text.is_some() && !claude.text.as_ref().unwrap().is_empty());
}
