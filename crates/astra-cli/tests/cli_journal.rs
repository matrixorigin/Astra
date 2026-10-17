use astra_credentials::local_profile_owner_id;
use astra_services::{
    OwnerScope,
    session_journal::{JournalDirGuard, journal_file_path_for_owner},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

fn run_journal(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_astra"));
    for (key, _) in
        std::env::vars().filter(|(key, _)| key.starts_with("ASTRA_") || key.starts_with("MOI_"))
    {
        command.env_remove(key);
    }
    let mut child = command
        .arg("journal")
        .args(args)
        .current_dir(root)
        .env("HOME", root)
        .env("ASTRA_LOCAL_STATE_ROOT", root.join("state"))
        .env("ASTRA_CLI_CREDENTIALS_DIR", root.join("credentials"))
        .env("MOI_AUTH_DIR", root.join("auth"))
        .env("ASTRA_CONFIG_SOURCE", "explicit-env")
        .env("ASTRA_API_URL", "http://127.0.0.1:9")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("journal command timed out: {args:?}: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn journal_commands_share_terminal_accounting_through_the_cli() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original: Vec<Value> =
        include_str!("../../services/fixtures/real_session_0ac769_min.jsonl")
            .lines()
            .map(|line| {
                let mut event: Value = serde_json::from_str(line).unwrap();
                // This test isolates accounting with explicitly scoped records.
                // Original-byte legacy session binding is covered separately by #1036.
                if event["type"] == "llm_round" && event["session_id"].is_null() {
                    event["session_id"] = json!(sid);
                }
                event
            })
            .collect();
    for terminal in [Some((33659, 2855)), Some((223453, 5330)), None] {
        let mut events = original.clone();
        if let Some((input, output)) = terminal {
            let turn = events
                .iter_mut()
                .find(|event| event["type"] == "turn")
                .unwrap();
            turn["tokens_in"] = json!(input);
            turn["tokens_out"] = json!(output);
        } else {
            events.retain(|event| event["type"] != "turn");
        }
        let content = events
            .iter()
            .map(|event| format!("{event}\n"))
            .collect::<String>();
        std::fs::write(&path, content + "not json\n").unwrap();
        let parse = |args: &[&str]| {
            let output = run_journal(root.path(), args);
            assert!(output.status.success(), "{args:?}: {output:?}");
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        };
        let digest = parse(&["digest", sid, "--format", "json", "--focus", "summary"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        let tree = parse(&["tree", sid, "--format", "json"]);
        let (input, output) = terminal.unwrap_or((0, 0));
        assert_eq!(digest["aggregates"]["total_tokens_in"], input);
        assert_eq!(digest["aggregates"]["total_tokens_out"], output);
        assert_eq!(digest["journal_lines_malformed"], 1);
        assert_eq!(
            digest["usage_coverage"]["inclusive_totals_are_billing"],
            false
        );
        assert_eq!(diff["a_tokens"]["prompt"], input);
        assert_eq!(diff["a_tokens"]["completion"], output);
        assert_eq!(diff["token_deltas"]["total"], 0);
        assert_eq!(
            tree["root"]["self_prompt_tokens"].as_u64().unwrap_or(0),
            input
        );
        assert_eq!(
            tree["root"]["self_completion_tokens"].as_u64().unwrap_or(0),
            output
        );
    }
    let foreign = json!({"type":"llm_round", "ts":"t", "session_id":"foreign-session"});
    std::fs::write(&path, format!("{}\n{foreign}\n", original[0])).unwrap();
    let output = run_journal(root.path(), &["digest", sid, "--format", "json"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another session"));
}
