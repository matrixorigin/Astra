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
    for terminal in [Some((33659, 2855, 12)), Some((223453, 5330, 541)), None] {
        let mut events = original.clone();
        if let Some((input, output, tools)) = terminal {
            let turn = events
                .iter_mut()
                .find(|event| event["type"] == "turn")
                .unwrap();
            turn["tokens_in"] = json!(input);
            turn["tokens_out"] = json!(output);
            turn["tool_count"] = json!(tools);
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
        let (input, output, tools) = terminal.unwrap_or((0, 0, 0));
        assert_eq!(digest["aggregates"]["total_tokens_in"], input);
        assert_eq!(digest["aggregates"]["total_tokens_out"], output);
        if terminal.is_none() {
            assert_eq!(digest["aggregates"]["root_tool_totals_complete"], false);
            assert_eq!(digest["aggregates"]["reported_root_tool_calls"], 0);
            assert_eq!(digest["aggregates"]["observed_root_tool_requests"], 12);
            let text = run_journal(
                root.path(),
                &["digest", sid, "--format", "text", "--focus", "summary"],
            );
            assert!(text.status.success());
            let text = String::from_utf8(text.stdout).unwrap();
            assert!(text.contains("tool_calls=?"), "{text}");
            assert!(
                text.contains("root=12 children=0 (not execution counts)"),
                "{text}"
            );
        }
        assert_eq!(digest["journal_lines_malformed"], 1);
        assert_eq!(
            digest["usage_coverage"]["inclusive_totals_are_billing"],
            false
        );
        assert_eq!(diff["a_tokens"]["prompt"], input);
        assert_eq!(diff["a_tokens"]["completion"], output);
        assert!(diff["token_deltas"]["total"].is_null());
        assert_eq!(diff["a_tokens"]["prompt_complete"], false);
        assert_eq!(
            tree["root"]["self_prompt_tokens"].as_u64().unwrap_or(0),
            input
        );
        assert_eq!(
            tree["root"]["self_completion_tokens"].as_u64().unwrap_or(0),
            output
        );
        assert_eq!(tree["root"]["tool_calls"].as_u64().unwrap_or(0), tools);
        assert_eq!(tree["root"]["self_prompt_tokens_complete"], false);
        let text = run_journal(root.path(), &["tree", sid]);
        assert!(text.status.success(), "{text:?}");
        let displayed = if tools == 0 {
            "?".to_owned()
        } else {
            format!("partial({tools})")
        };
        assert!(
            String::from_utf8_lossy(&text.stdout).contains(&format!("tool_calls={displayed}\n"))
        );
    }
    std::fs::write(
        &path,
        format!(
            "{}\n",
            json!({"type":"turn", "ts":"t", "session_id":sid,
        "turn":1, "tokens_in":0, "tokens_out":0, "cache_read_tokens":0,
        "cache_creation_tokens":0, "tool_count":0})
        ),
    )
    .unwrap();
    let clean = run_journal(root.path(), &["diff", sid, sid, "--format", "json"]);
    assert!(clean.status.success(), "{clean:?}");
    let clean: Value = serde_json::from_slice(&clean.stdout).unwrap();
    assert_eq!(clean["token_deltas"]["total"], 0);
    assert_eq!(clean["a_tokens"]["total_complete"], true);
    let foreign = json!({"type":"llm_round", "ts":"t", "session_id":"foreign-session"});
    std::fs::write(&path, format!("{}\n{foreign}\n", original[0])).unwrap();
    let output = run_journal(root.path(), &["digest", sid, "--format", "json"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another session"));
}

#[test]
fn unavailable_account_journal_preserves_local_analysis_and_reports_error() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "63d977e8-c953-4d6b-8d74-19686f8ce522";
    let account = "journal-test-account";
    astra_credentials::CredentialStore::with_path(root.path().join("credentials/credentials.json"))
        .mutate(|credentials| {
            credentials.current_profile = Some("default".into());
            credentials.profiles.insert(
                "default".into(),
                astra_credentials::Profile {
                    account_id: Some(account.into()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    let local =
        OwnerScope::user(local_profile_owner_id("default", Some(account)).unwrap()).unwrap();
    let local_path = journal_file_path_for_owner(&local, sid).unwrap();
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, format!(
        "{{\"type\":\"turn\",\"ts\":\"t1\",\"session_id\":\"{sid}\",\"turn\":1,\"tokens_in\":100,\"tokens_out\":10,\"tool_count\":0}}\n"
    )).unwrap();
    let account_owner = OwnerScope::user(account).unwrap();
    let account_path = journal_file_path_for_owner(&account_owner, sid).unwrap();
    std::fs::create_dir_all(&account_path).unwrap();

    let parse = |args: &[&str]| {
        let output = run_journal(root.path(), args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let tree = parse(&["tree", sid, "--format", "json"]);
    assert_eq!(tree["root"]["self_prompt_tokens"], 100);
    assert_eq!(tree["root"]["self_prompt_tokens_complete"], false);
    assert_eq!(tree["source_errors"].as_array().unwrap().len(), 1);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tokens"]["prompt"], 100);
    assert_eq!(diff["a_observation_incomplete"], true);
    assert_eq!(diff["a_source_errors"].as_array().unwrap().len(), 1);
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "summary"]);
    assert_eq!(digest["aggregates"]["total_tokens_in"], 100);
    assert!(
        digest["journal_source_coverage"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["read_error"].is_string())
    );

    // A readable foreign-session source is an identity violation, not an optional I/O gap.
    std::fs::remove_dir(&account_path).unwrap();
    std::fs::write(
        &account_path,
        "{\"type\":\"turn\",\"ts\":\"t1\",\"session_id\":\"other-session\"}\n",
    )
    .unwrap();
    let invalid = run_journal(root.path(), &["tree", sid, "--format", "json"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("another session"));

    std::fs::write(&account_path, [0xff]).unwrap();
    let corrupt = run_journal(root.path(), &["tree", sid, "--format", "json"]);
    assert!(
        !corrupt.status.success(),
        "invalid UTF-8 is corruption, not an unavailable source"
    );
}

#[test]
fn replayed_and_conflicting_terminals_never_inflate_analysis() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "b046daab-1a40-4b9d-9280-98ec06cf3e77";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let terminal = json!({"type":"turn", "ts":"t1", "session_id":sid, "turn":1,
        "metadata":{"run_id":"root-run"}, "tokens_in":100, "tokens_out":10,
        "cache_read_tokens":0, "cache_creation_tokens":0, "tool_count":3});
    let parse = |args: &[&str]| {
        let output = run_journal(root.path(), args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    for conflicting in [false, true] {
        let mut replay = terminal.clone();
        if conflicting {
            replay["tokens_in"] = json!(200);
        }
        std::fs::write(&path, format!("{terminal}\n{replay}\n")).unwrap();
        let digest = parse(&["digest", sid, "--format", "json"]);
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        let expected = if conflicting { 0 } else { 100 };
        assert_eq!(digest["aggregates"]["total_tokens_in"], expected);
        assert_eq!(digest["conflicting_terminal_count"], u8::from(conflicting));
        assert_eq!(tree["schema_version"], "astra-journal-tree-v2");
        assert_eq!(tree["root"]["self_prompt_tokens"], expected);
        assert_eq!(diff["a_tokens"]["prompt"], expected);
        if conflicting {
            assert_eq!(tree["root"]["self_prompt_tokens_complete"], false);
            assert!(diff["token_deltas"]["total"].is_null());
        } else {
            assert!(digest["turns"][0]["tool_calls_ok"].is_null());
            assert!(digest["turns"][0]["tool_calls_fresh"].is_null());
        }
    }
}

#[cfg(unix)]
#[test]
fn missing_journal_source_cannot_return_an_empty_analysis() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "7d546d51-b229-4544-b22f-114bcd29a819";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(root.path().join("missing"), &path).unwrap();
    let tree = run_journal(root.path(), &["tree", sid, "--format", "json"]);
    assert!(!tree.status.success(), "{tree:?}");
    assert!(String::from_utf8_lossy(&tree.stderr).contains("journal not found"));
}

#[test]
fn unqualified_failure_usage_never_becomes_a_complete_total() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "29113b97-4623-4d23-a7a0-810c69bf0f49";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut event = json!({"type":"turn_error", "ts":"t", "session_id":sid, "turn":1,
        "metadata":{"run_id":"root-run"}, "tokens_in":100, "tokens_out":10,
        "cache_read_tokens":0, "cache_creation_tokens":0, "tool_count":0});
    let parse = |args: &[&str]| {
        let output = run_journal(root.path(), args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    for (qualified, conflicting) in [(false, false), (true, false), (true, true)] {
        if qualified {
            event["metadata"]["token_usage_qualified"] = json!(true);
        }
        if conflicting {
            event["metadata"]["qualified_usage"] = json!({});
        }
        std::fs::write(&path, format!("{event}\n")).unwrap();
        let digest = parse(&["digest", sid, "--format", "json"]);
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(
            digest["aggregates"]["total_tokens_in"],
            if qualified && !conflicting { 100 } else { 0 }
        );
        assert_eq!(
            tree["root"]["self_prompt_tokens_complete"],
            qualified && !conflicting
        );
        assert_eq!(
            diff["a_tokens"]["prompt_complete"],
            qualified && !conflicting
        );
        assert_eq!(
            digest["turn_errors"][0]["usage_source"],
            if qualified && !conflicting {
                "terminal_qualified"
            } else {
                "unqualified_observation"
            }
        );
    }
}
