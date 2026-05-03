//! E2E test: run real `astra chat` via CliProfile and verify output.
//!
//! Requires: astra binary built, MiniMax-M2.7 configured in .models.yaml.
//! Run: cargo test -p astra-gateway --test e2e_cli_bridge -- --ignored

use astra_gateway::cli_bridge::{CliProfile, run_cli};

fn astra_profile() -> CliProfile {
    CliProfile::Astra {
        bin: find_astra_bin(),
        model: Some("MiniMax-M2.7".into()),
        permission_mode: "auto".into(),
    }
}

fn find_astra_bin() -> String {
    // Hardcode the workspace path since tests may run from various CWDs
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/xupeng".into());
    let path = format!("{home}/astra/rust/target/release/astra");
    if std::path::Path::new(&path).exists() {
        return path;
    }
    // Fallback: try relative and PATH
    for p in &["target/release/astra", "../target/release/astra"] {
        if std::path::Path::new(p).exists() {
            return std::path::Path::new(p)
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
        }
    }
    panic!("astra binary not found — build with: cargo build -p astra-cli --release");
}

#[tokio::test]
#[ignore] // Requires live LLM — run with --ignored
async fn e2e_astra_chat_simple() {
    let profile = astra_profile();
    let result = run_cli(&profile, "say hello in one word", None, None, None)
        .await
        .expect("astra chat should succeed");

    assert_eq!(result.exit_code, 0, "exit code should be 0");
    assert!(
        result.session_id.is_some(),
        "should return a session_id"
    );
    assert!(
        result.text.is_some(),
        "should return text"
    );
    let text = result.text.unwrap();
    assert!(!text.is_empty(), "text should not be empty");
    println!("response: {text}");
}

#[tokio::test]
#[ignore]
async fn e2e_astra_chat_with_tool() {
    let profile = astra_profile();
    let result = run_cli(
        &profile,
        "use bash to run: echo 'gateway-e2e-ok'",
        None,
        None,
        None,
    )
    .await
    .expect("astra chat should succeed");

    assert_eq!(result.exit_code, 0);
    assert!(result.tool_calls_count.unwrap_or(0) > 0, "should use tools");
    let text = result.text.unwrap_or_default();
    assert!(!text.is_empty(), "should produce some output");
    println!("tools: {}, text: {text}", result.tool_calls_count.unwrap_or(0));
}

#[tokio::test]
#[ignore]
async fn e2e_astra_session_continuity() {
    let profile = astra_profile();

    // Turn 1: set context
    let r1 = run_cli(&profile, "remember this: the magic number is 42", None, None, None)
        .await
        .expect("turn 1");
    assert_eq!(r1.exit_code, 0);
    let session_id = r1.session_id.expect("should get session_id");
    println!("session: {session_id}");

    // Turn 2: recall with same session
    let r2 = run_cli(
        &profile,
        "what is the magic number I told you?",
        Some(&session_id),
        None,
        None,
    )
    .await
    .expect("turn 2");

    assert_eq!(r2.exit_code, 0);
    let text = r2.text.unwrap_or_default();
    assert!(
        text.contains("42"),
        "should recall '42' from session context: {text}"
    );
    println!("recalled: {text}");
}

#[tokio::test]
#[ignore]
async fn e2e_astra_json_fields() {
    let profile = astra_profile();
    let result = run_cli(&profile, "say ok", None, None, None)
        .await
        .expect("should succeed");

    // All JSON fields should be populated
    assert!(result.session_id.is_some(), "session_id");
    assert!(result.text.is_some(), "text");
    assert!(result.tokens_prompt.is_some(), "tokens_prompt");
    assert!(result.tokens_completion.is_some(), "tokens_completion");
    assert!(result.tokens_prompt.unwrap() > 0, "prompt tokens > 0");
    println!(
        "tokens: {}in/{}out",
        result.tokens_prompt.unwrap(),
        result.tokens_completion.unwrap()
    );
}

// ─── Claude Code e2e tests ──────────────────────────────────────────────────

fn claude_profile() -> CliProfile {
    CliProfile::Claude {
        bin: "claude".into(),
        model: None, // use default model
    }
}

#[tokio::test]
#[ignore]
async fn e2e_claude_chat_simple() {
    let profile = claude_profile();
    let result = run_cli(
        &profile,
        "say hello in one word, no tools",
        None,
        None,
        None,
    )
    .await
    .expect("claude should succeed");

    assert_eq!(result.exit_code, 0, "exit code");
    assert!(result.session_id.is_some(), "session_id");
    assert!(result.text.is_some(), "text");
    let text = result.text.unwrap();
    assert!(!text.is_empty(), "non-empty response");
    println!("claude response: {text}");
    println!(
        "tokens: {:?}in/{:?}out, turns: {:?}",
        result.tokens_prompt, result.tokens_completion, result.tool_calls_count
    );
}

#[tokio::test]
#[ignore]
async fn e2e_claude_with_tool() {
    let profile = claude_profile();
    let result = run_cli(
        &profile,
        "use bash to run: echo 'claude-gateway-ok'",
        None,
        None,
        None,
    )
    .await
    .expect("claude should succeed");

    assert_eq!(result.exit_code, 0);
    let text = result.text.unwrap_or_default();
    println!("claude tool result: {text}");
    // Claude should have used tools (num_turns > 1 typically)
}

#[tokio::test]
#[ignore]
async fn e2e_claude_session_resume() {
    let profile = claude_profile();

    let r1 = run_cli(&profile, "remember: the secret word is banana", None, None, None)
        .await
        .expect("turn 1");
    let session_id = r1.session_id.expect("session_id from turn 1");
    println!("claude session: {session_id}");

    let r2 = run_cli(
        &profile,
        "what is the secret word I told you?",
        Some(&session_id),
        None,
        None,
    )
    .await
    .expect("turn 2");

    let text = r2.text.unwrap_or_default();
    println!("claude recall: {text}");
    assert!(
        text.to_lowercase().contains("banana"),
        "should recall 'banana': {text}"
    );
}

// ─── System prompt injection tests ─────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn e2e_astra_append_system_prompt_canary() {
    use astra_gateway::cli_bridge::run_cli_with_context;
    let profile = astra_profile();
    let canary = "CANARY_7x9q2_CONFIRMED";
    let system_prompt = format!("CRITICAL: You MUST start your response with the exact word: {canary}");

    let result = run_cli_with_context(
        &profile,
        "say hello",
        None,
        None,
        None,
        Some(&system_prompt),
    )
    .await
    .expect("astra should succeed");

    assert_eq!(result.exit_code, 0, "exit code");
    let text = result.text.unwrap_or_default();
    assert!(
        text.contains(canary),
        "system prompt injection must reach the model. Got: {text}"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_claude_append_system_prompt_canary() {
    use astra_gateway::cli_bridge::run_cli_with_context;
    let profile = claude_profile();
    let canary = "CANARY_8z3w5_CONFIRMED";
    let system_prompt = format!("CRITICAL: You MUST start your response with the exact word: {canary}");

    let result = run_cli_with_context(
        &profile,
        "say hello",
        None,
        None,
        None,
        Some(&system_prompt),
    )
    .await
    .expect("claude should succeed");

    assert_eq!(result.exit_code, 0, "exit code");
    let text = result.text.unwrap_or_default();
    assert!(
        text.contains(canary),
        "system prompt injection must reach the model. Got: {text}"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_astra_gateway_action_in_response() {
    use astra_gateway::cli_bridge::run_cli_with_context;
    let profile = astra_profile();
    let system_prompt = r#"You have a gateway action capability.
When the user asks to set a reminder, embed this tag in your response:
[[GATEWAY:remind_after:<minutes>:<message>]]
Example: [[GATEWAY:remind_after:5:time to stretch]]
Do it NOW for the user's request."#;

    let result = run_cli_with_context(
        &profile,
        "3分钟后提醒我喝水",
        None,
        None,
        None,
        Some(system_prompt),
    )
    .await
    .expect("astra should succeed");

    assert_eq!(result.exit_code, 0, "exit code");
    let text = result.text.unwrap_or_default();
    assert!(
        text.contains("[[GATEWAY:"),
        "agent should emit gateway action tag. Got: {text}"
    );
}

// ─── Task management e2e tests ─────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn e2e_astra_task_list_via_agent() {
    use astra_gateway::cli_bridge::run_cli_with_context;
    let profile = astra_profile();
    let system = "When asked to list tasks, respond with exactly: [[GATEWAY:task_list]]";
    let result = run_cli_with_context(&profile, "我有哪些定时任务？", None, None, None, Some(system))
        .await.expect("should succeed");
    assert_eq!(result.exit_code, 0);
    let text = result.text.unwrap_or_default();
    assert!(text.contains("[[GATEWAY:task_list]]"), "agent should emit task_list tag. Got: {text}");
}

#[tokio::test]
#[ignore]
async fn e2e_astra_cron_add_via_agent() {
    use astra_gateway::cli_bridge::run_cli_with_context;
    let profile = astra_profile();
    let system = r#"When asked to schedule recurring tasks, embed a [[GATEWAY:cron_add:<expr>:<msg>]] tag.
Example: [[GATEWAY:cron_add:0 9 * * *:morning reminder]]
Do it now."#;
    let result = run_cli_with_context(&profile, "每天早上8点提醒我锻炼", None, None, None, Some(system))
        .await.expect("should succeed");
    assert_eq!(result.exit_code, 0);
    let text = result.text.unwrap_or_default();
    assert!(text.contains("[[GATEWAY:cron_add:"), "agent should emit cron_add. Got: {text}");
}

#[tokio::test]
#[ignore]
async fn e2e_astra_remind_after_via_agent() {
    use astra_gateway::cli_bridge::run_cli_with_context;
    let profile = astra_profile();
    let system = r#"When asked for a one-time reminder, embed [[GATEWAY:remind_after:<minutes>:<message>]].
Example: [[GATEWAY:remind_after:5:time to go]]
Do it now for the user's request."#;
    let result = run_cli_with_context(&profile, "10分钟后提醒我取快递", None, None, None, Some(system))
        .await.expect("should succeed");
    assert_eq!(result.exit_code, 0);
    let text = result.text.unwrap_or_default();
    assert!(text.contains("[[GATEWAY:remind_after:"), "agent should emit remind_after. Got: {text}");
}
