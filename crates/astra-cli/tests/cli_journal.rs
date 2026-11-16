use astra_credentials::local_profile_owner_id;
use astra_services::{
    OwnerScope,
    session_journal::{JournalDirGuard, journal_file_path_for_owner},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

fn run_journal(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_astra"));
    // Capture to files so a detailed journal cannot fill a pipe while the
    // parent waits to enforce the timeout.
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
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
        .stdout(Stdio::from(stdout.try_clone().unwrap()))
        .stderr(Stdio::from(stderr.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("journal command timed out: {args:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().unwrap();
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    stdout.read_to_end(&mut output.stdout).unwrap();
    stderr.read_to_end(&mut output.stderr).unwrap();
    output
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
    let terminal = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"root"},"tokens_in":100,"tokens_out":10,
        "cache_read_tokens":0,"cache_creation_tokens":0,"tool_count":0,
        "tool_outcomes":{"requested":0,"executed":0,"succeeded":0,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0}});
    std::fs::write(&local_path, format!("{terminal}\n")).unwrap();
    let account_owner = OwnerScope::user(account).unwrap();
    let account_path = journal_file_path_for_owner(&account_owner, sid).unwrap();
    std::fs::create_dir_all(account_path.parent().unwrap()).unwrap();

    let parse = |args: &[&str]| {
        let output = run_journal(root.path(), args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    // The callback was retained by the supplemental owner. It remains an
    // observed child execution after the authorized journal join.
    let callback = json!({"type":"sync_marker","ts":"t2","session_id":sid,
        "producer_scope":{"run_id":"child"},"metadata":{"source":"child_agent"},
        "tool_calls":[{"name":"read_file","ok":true,"ms":1,"tool_call_id":"request",
            "invocation_identity":{"user_id":account,"session_id":sid,"run_id":"child",
                "turn_chain_id":"chain","invocation_id":"request"}}]});
    std::fs::write(&account_path, format!("{callback}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["self_prompt_tokens_complete"], true);
    assert_eq!(tree["root"]["prompt_tokens_complete"], false);
    assert_eq!(tree["root"]["tool_calls"], 1);
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_calls_executed"], 1);
    assert_eq!(diff["a_tool_sequence_complete"], false);

    let mut duplicate = callback.clone();
    duplicate["ts"] = json!("t3");
    std::fs::write(&local_path, format!("{terminal}\n{duplicate}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 1);
    assert_eq!(diff["a_tool_calls_executed"], 1);

    std::fs::write(&local_path, format!("{terminal}\n")).unwrap();
    std::fs::remove_file(&account_path).unwrap();
    std::fs::create_dir(&account_path).unwrap();
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

    // A flush annotation must not erase a retained failure or manufacture
    // another attempt. Different terminal reports must retain both errors,
    // without certifying incompatible outcomes for the same invocation.
    let identity = json!({"user_id":"owner","session_id":sid,"run_id":"root-run",
        "turn_chain_id":"chain","invocation_id":"a"});
    let failed = json!({"name":"bash","ok":false,"ms":1,"tool_call_id":"a",
        "invocation_identity":identity,"disposition":"executed","error":"compiler failed"});
    let error = json!({"type":"turn_error","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"root-run"},"error":"provider timed out","tool_calls":[failed]});
    let mut annotated = error.clone();
    annotated["metadata"] = json!({"partial":true});
    let mut dropped = error.clone();
    dropped["metadata"] = json!({"dropped_events_before":3});
    for replay in [error.clone(), annotated, dropped] {
        for (first, second) in [(&error, &replay), (&replay, &error)] {
            std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
            let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
            assert_eq!(digest["conflicting_terminal_count"], 0, "replay={replay}");
            assert_eq!(digest["aggregates"]["turn_error_count"], 1);
            assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 1);
            assert!(digest.to_string().contains("provider timed out"));
            assert!(digest.to_string().contains("compiler failed"));
        }
    }
    let mut malformed = error.clone();
    malformed["metadata"] = json!({"partial":"true"});
    std::fs::write(&path, format!("{error}\n{malformed}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["conflicting_terminal_count"], 1);
    assert!(digest.to_string().contains("provider timed out"));

    let mut invalid_scope = error.clone();
    invalid_scope["metadata"] = json!({"run_id":"different-run"});
    std::fs::write(&path, format!("{invalid_scope}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["conflicting_terminal_count"], 1);
    assert!(digest.to_string().contains("provider timed out"));
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tool_calls_executed"], 0);
    assert_eq!(diff["a_tool_sequence_complete"], false);
    let mut conflicting_error = error.clone();
    conflicting_error["error"] = json!("stream ended");
    conflicting_error["tool_calls"][0]["disposition"] = json!("rejected");
    conflicting_error["tool_calls"][0]["error"] = json!("access denied");
    std::fs::write(&path, format!("{error}\n{conflicting_error}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["conflicting_terminal_count"], 1);
    for evidence in [
        "provider timed out",
        "stream ended",
        "compiler failed",
        "access denied",
    ] {
        assert!(
            digest.to_string().contains(evidence),
            "missing {evidence}: {digest}"
        );
    }
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tool_calls_executed"], 0);
    assert_eq!(diff["a_tool_sequence_complete"], false);

    // Identical execution facts on conflicting terminals still witness one
    // call; a later partial aggregate cannot prove five more distinct calls.
    let mut second_failure = error.clone();
    second_failure["error"] = json!("stream ended");
    let mut independent = failed.clone();
    independent["ok"] = json!(true);
    independent["tool_call_id"] = json!("b");
    independent["invocation_identity"]["run_id"] = json!("later-run");
    independent["invocation_identity"]["invocation_id"] = json!("b");
    independent.as_object_mut().unwrap().remove("error");
    let later = json!({"type":"turn","ts":"t3","session_id":sid,"turn":2,
        "producer_scope":{"run_id":"later-run"},"tool_count":5,
        "tool_outcomes":{"requested":5,"executed":5,"succeeded":5,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[independent]});
    std::fs::write(&path, format!("{error}\n{second_failure}\n{later}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 2);
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_calls_executed"], 2);
    assert_eq!(diff["a_tool_sequence_complete"], false);

    // Summary-only rejections can overlap anonymous executions. The
    // identified independent-call control is exercised above.
    let rejected_summary = json!({"requested":1,"executed":0,"succeeded":0,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":1,"reused":0,"suppressed":0,"deferred":0});
    let rejection_a = json!({"type":"turn_error","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"root-run"},"tool_count":0,
        "tool_outcomes":rejected_summary,"error":"one"});
    let mut rejection_b = rejection_a.clone();
    rejection_b["error"] = json!("two");
    let anonymous_round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"root-run"},
        "tool_calls":[{"name":"bash","ok":true,"ms":1,"disposition":"executed"}]});
    std::fs::write(
        &path,
        format!("{anonymous_round}\n{rejection_a}\n{rejection_b}\n"),
    )
    .unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0);
    assert_eq!(diff["a_tool_calls_executed"], 0);
    assert_eq!(diff["a_tool_sequence_complete"], false);
    // A summary-only rejection can also overlap an identified execution.
    let mut identified_round = anonymous_round.clone();
    identified_round["tool_calls"][0]["tool_call_id"] = json!("a");
    identified_round["tool_calls"][0]["invocation_identity"] = identity.clone();
    std::fs::write(
        &path,
        format!("{identified_round}\n{rejection_a}\n{rejection_b}\n"),
    )
    .unwrap();
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tool_calls_executed"], 0);

    // The same ambiguity exists without conflicting terminal copies.
    std::fs::write(&path, format!("{identified_round}\n{rejection_a}\n")).unwrap();
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tool_calls_executed"], 0);
    assert_eq!(diff["a_tool_sequence_complete"], false);

    // A fully identified rejection does not erase a distinct execution.
    let mut known_rejection_a = rejection_a.clone();
    let mut rejected_call = failed.clone();
    rejected_call["disposition"] = json!("rejected");
    known_rejection_a["tool_calls"] = json!([rejected_call]);
    let mut known_rejection_b = known_rejection_a.clone();
    known_rejection_b["error"] = json!("two");
    let independent_round = json!({"type":"llm_round","ts":"t3","session_id":sid,"turn":2,"round":0,
        "producer_scope":{"run_id":"later-run"},"tool_calls":[independent]});
    std::fs::write(
        &path,
        format!("{known_rejection_a}\n{known_rejection_b}\n{independent_round}\n"),
    )
    .unwrap();
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tool_calls_executed"], 1);

    // A root ledger does not include a child callback's tool executions.
    let child_identity = json!({"user_id":"owner","session_id":sid,"run_id":"child-run",
        "turn_chain_id":"child-chain","invocation_id":"child-call"});
    let child_call = json!({"name":"bash","ok":true,"ms":1,"tool_call_id":"child-call",
        "invocation_identity":child_identity,"disposition":"executed"});
    let child_callback = json!({"type":"sync_marker","ts":"t1","session_id":sid,
        "producer_scope":{"run_id":"child-run"},"metadata":{"source":"child_agent"},
        "tool_calls":[child_call]});
    for terminals in [vec![&rejection_a], vec![&rejection_a, &rejection_b]] {
        let mut journal = format!("{child_callback}\n");
        for terminal in terminals {
            journal.push_str(&format!("{terminal}\n"));
        }
        std::fs::write(&path, journal).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], 1);
        assert_eq!(diff["a_tool_calls_executed"], 1);
        assert_eq!(diff["a_tool_sequence_complete"], false);
    }
    let mut anonymous_child = child_callback.clone();
    anonymous_child["tool_calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("invocation_identity");
    std::fs::write(&path, format!("{anonymous_child}\n{rejection_a}\n")).unwrap();
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tool_calls_executed"], 1);
    // A rejection explicitly identifying that child call still contradicts it.
    let mut explicit_child_rejection = rejection_a.clone();
    let mut rejected_child = child_call.clone();
    rejected_child["disposition"] = json!("rejected");
    rejected_child["ok"] = json!(false);
    explicit_child_rejection["tool_calls"] = json!([rejected_child]);
    std::fs::write(
        &path,
        format!("{child_callback}\n{explicit_child_rejection}\n"),
    )
    .unwrap();
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tool_calls_executed"], 0);

    // The round is still owned by the conflicted attempt, and its only
    // failure is visible. Anonymous copies are not added across carriers.
    let mut failed_round = anonymous_round.clone();
    failed_round["tool_calls"][0]["ok"] = json!(false);
    failed_round["tool_calls"][0]["error"] = json!("round failed");
    std::fs::write(
        &path,
        format!("{failed_round}\n{rejection_a}\n{rejection_b}\n"),
    )
    .unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 1);
    assert!(digest.to_string().contains("round failed"));
    let mut anon_a = rejection_a.clone();
    anon_a["tool_calls"] = failed_round["tool_calls"].clone();
    let mut anon_b = anon_a.clone();
    anon_b["error"] = json!("two");
    std::fs::write(&path, format!("{anon_a}\n{anon_b}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 1);
    anon_a["tool_calls"]
        .as_array_mut()
        .unwrap()
        .push(failed_round["tool_calls"][0].clone());
    std::fs::write(&path, format!("{anon_a}\n{anon_b}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 2);

    // Conflicting round carriers replay anonymous facts without increasing
    // the failure count; repetitions within one carrier remain distinct.
    let mut failed_round_replay = failed_round.clone();
    failed_round_replay["duration_ms"] = json!(101);
    std::fs::write(
        &path,
        format!("{failed_round}\n{failed_round_replay}\n{rejection_a}\n"),
    )
    .unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 1);
    assert_eq!(digest["failed_tool_calls"].as_array().unwrap().len(), 1);
    let mut repeated = failed_round.clone();
    repeated["tool_calls"]
        .as_array_mut()
        .unwrap()
        .push(failed_round["tool_calls"][0].clone());
    std::fs::write(
        &path,
        format!("{repeated}\n{failed_round_replay}\n{rejection_a}\n"),
    )
    .unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 2);

    // Conflicting attempts have no terminal row, so their failure details
    // remain unsequenced diagnostic observations.
    let mut later_a = anon_a.clone();
    later_a["turn"] = json!(2);
    later_a["producer_scope"]["run_id"] = json!("later-run");
    later_a["ts"] = json!("t3");
    later_a["tool_calls"][0]["error"] = json!("later failure");
    later_a["tool_calls"].as_array_mut().unwrap().truncate(1);
    let mut later_b = later_a.clone();
    later_b["error"] = json!("later conflicting error");
    std::fs::write(&path, format!("{anon_a}\n{anon_b}\n{later_a}\n{later_b}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert!(digest["other_errors"].to_string().contains("later failure"));
    assert!(digest.get("failed_tool_calls").is_none());

    // Candidate terminal identity anchors child topology, not its aggregates.
    let child = json!({"type":"delegation_sub_run_started","ts":"t1","session_id":sid,
        "metadata":{"delegation_id":"d","sub_run_id":"child-run","parent_run_id":"root-run",
            "agent_id":"worker","status":"running","depth":1}});
    std::fs::write(&path, format!("{child}\n{rejection_a}\n{rejection_b}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    assert_eq!(tree["root"]["children"][0]["id"], "child-run");
    assert_eq!(tree["root"]["tool_calls_complete"], false);

    let mut successful = error.clone();
    successful["tool_calls"][0]["ok"] = json!(true);
    successful["tool_calls"][0]["result_full"] = json!("successful secret output");
    successful["tool_calls"][0]["args_preview"] = json!("sensitive args");
    successful["tool_calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("error");
    let mut successful_conflict = successful.clone();
    successful_conflict["error"] = json!("stream ended");
    std::fs::write(&path, format!("{successful}\n{successful_conflict}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert!(digest["other_errors"].to_string().contains("bash"));
    let summary = parse(&["digest", sid, "--format", "json", "--focus", "summary"]);
    assert!(!summary.to_string().contains("successful secret output"));
    assert!(!summary.to_string().contains("sensitive args"));
    assert_eq!(diff["a_tool_calls_executed"], 1);
    assert_eq!(diff["a_tool_sequence"], json!(["bash"]));
    assert_eq!(diff["a_tool_sequence_complete"], false);

    let mut unscoped = invalid_scope.clone();
    unscoped.as_object_mut().unwrap().remove("turn");
    std::fs::write(&path, format!("{unscoped}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["total_events"], 1);
    assert_eq!(tree["skipped_events"], 1);
    assert_eq!(diff["a_event_counts"]["turn_error"], 1);
    assert_eq!(diff["a_observation_incomplete"], true);

    // A superseded provider response may report requests without per-call
    // details. Different timings do not contradict an agreed request count.
    let round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,
        "round":0,"producer_scope":{"run_id":"root-run"},"duration_ms":100,
        "tool_calls_returned":3,
        "metadata":{"source":"agentic_loop","finish_reason":"superseded_by_runtime_input"}});
    let outcome = json!({"requested":3,"executed":0,"succeeded":0,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":3,"reused":0,"suppressed":0,"deferred":0});
    let terminal = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"root-run"},"tool_count":0,"tool_outcomes":outcome,"llm_rounds":1});
    for duration in [100, 101] {
        let mut replay = round.clone();
        replay["duration_ms"] = json!(duration);
        std::fs::write(&path, format!("{round}\n{replay}\n{terminal}\n")).unwrap();
        let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
        assert_eq!(digest["conflicting_round_count"], u8::from(duration != 100));
        assert_eq!(digest["aggregates"]["observed_root_tool_requests"], 3);
        if duration != 100 {
            assert!(
                digest["turns"][0]["llm_round_details"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|row| row["duration_ms"].is_null())
            );
        }
    }

    // Conflicting producer identities are retained as raw observations, but
    // cannot be attributed to this root or counted twice as its requests.
    let mut wrong_scope = round.clone();
    wrong_scope["metadata"]["run_id"] = json!("different-run");
    let mut wrong_scope_replay = wrong_scope.clone();
    wrong_scope_replay["duration_ms"] = json!(101);
    std::fs::write(
        &path,
        format!("{wrong_scope}\n{wrong_scope_replay}\n{terminal}\n"),
    )
    .unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["aggregates"]["observed_root_tool_requests"], 0);
    assert_eq!(digest["aggregates"]["root_tool_counts_complete"], false);
    assert_eq!(digest["unattributed_root_round_count"], 0);
    assert!(
        digest["other_errors"]
            .to_string()
            .contains("conflicting_round_scope")
    );
    assert!(digest["other_errors"].to_string().contains("different-run"));
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(diff["a_tool_sequence_complete"], false);

    let mut inconsistent = round.clone();
    inconsistent["tool_calls_returned"] = json!(4);
    std::fs::write(&path, format!("{round}\n{inconsistent}\n{terminal}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(digest["conflicting_round_count"], 1);
    assert_eq!(digest["aggregates"]["observed_root_tool_requests"], 0);
    assert!(
        digest["other_errors"]
            .to_string()
            .contains("\"observed_requests\":[3,4]")
    );

    // Conflicting terminal reports for one attempt are removed before any
    // projection. An independently retained round can still witness a call.
    let call = json!({"name":"bash","ok":true,"ms":1,"tool_call_id":"a",
        "invocation_identity":{"user_id":"owner","session_id":sid,"run_id":"root-run",
            "turn_chain_id":"chain","invocation_id":"a"}});
    let summary = |count| {
        json!({"requested":count,"executed":count,"succeeded":count,
        "failed":0,"started_unknown":0,"unknown":0,"rejected":0,"reused":0,
        "suppressed":0,"deferred":0})
    };
    let first = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"root-run"},"tool_count":1,"tool_outcomes":summary(1),
        "tool_calls":[call]});
    let mut conflicting = first.clone();
    conflicting["ts"] = json!("t2");
    conflicting["tool_count"] = json!(2);
    conflicting["tool_outcomes"] = summary(2);
    let round = json!({"type":"llm_round","ts":"t0","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"root-run"},"tool_calls":[call]});
    std::fs::write(&path, format!("{round}\n{first}\n{conflicting}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 1);
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_calls_executed"], 1);
    assert_eq!(diff["a_tool_sequence_complete"], false);
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

#[test]
fn digest_reports_only_observed_semantics_and_complete_round_averages() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "679d8c97-5509-4b30-bca6-c1fe0c013614";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let parse = || {
        let output = run_journal(
            root.path(),
            &["digest", sid, "--format", "json", "--focus", "all"],
        );
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let outcomes = json!({"requested":1,"executed":1,"succeeded":1,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0});
    // Arbitrary tool output, including truncated and redacted output, cannot
    // change the producer's recorded outcome.
    for body in [
        Some("hello"),
        Some(r#"{"ok":false,"feature_enabled":false}"#),
        Some(""),
        Some("<redacted: len=11 sha=abc>"),
        None,
    ] {
        let mut record = json!({"name":"bash","ok":true,"ms":1});
        if let Some(body) = body {
            record["result_preview"] = json!(body);
        }
        let event = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
            "tool_count":1,"tool_calls":[record],"tool_outcomes":outcomes,
            "llm_rounds":1,"tokens_in":1,"tokens_out":1,"cache_read_tokens":0,"cache_creation_tokens":0});
        std::fs::write(&path, format!("{event}\n")).unwrap();
        let digest = parse();
        assert_eq!(
            digest["aggregates"]["root_tool_totals_complete"], true,
            "body={body:?}"
        );
        assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 0);
        let group = &digest["turns"][0]["tool_groups"][0];
        assert_eq!(group["fail_count"], 0);
        assert_eq!(group["ok_count"], 1);
        assert_eq!(group["unknown_count"], 0);
        assert_eq!(digest["aggregates"]["avg_tool_calls_per_round"], 1.0);
    }
    for (disposition, executed, started_unknown, unknown) in
        [("unknown", 0, 0, 1), ("executed", 1, 1, 0)]
    {
        for kind in ["turn", "turn_error"] {
            let event = json!({"type":kind,"ts":"t1","session_id":sid,"turn":1,
                "tool_count":executed,"tool_calls":[{"name":"bash","ok":false,"ms":1,
                    "disposition":disposition,"error_kind":"tool_outcome_unknown"}],
                "tool_outcomes":{"requested":1,"executed":executed,"succeeded":0,"failed":0,
                    "started_unknown":started_unknown,"unknown":unknown,"rejected":0,
                    "reused":0,"suppressed":0,"deferred":0},"llm_rounds":1});
            std::fs::write(&path, format!("{event}\n")).unwrap();
            let digest = parse();
            assert_eq!(
                digest["aggregates"]["reported_root_tool_outcomes_unknown"],
                1
            );
            assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 0);
            assert_eq!(digest["aggregates"]["root_tool_totals_complete"], false);
            assert_eq!(
                digest["aggregates"]["root_tool_counts_complete"],
                disposition == "executed"
            );
            if disposition == "executed" {
                assert_eq!(digest["aggregates"]["avg_tool_calls_per_round"], 1.0);
            } else {
                assert!(digest["aggregates"]["avg_tool_calls_per_round"].is_null());
            }
            if kind == "turn" {
                assert_eq!(digest["turns"][0]["tool_groups"][0]["unknown_count"], 1);
            }
            let tree = run_journal(root.path(), &["tree", sid, "--format", "json"]);
            assert!(tree.status.success(), "{tree:?}");
            let tree: Value = serde_json::from_slice(&tree.stdout).unwrap();
            assert_eq!(
                tree["root"]["tool_calls_complete"],
                disposition == "executed"
            );
        }
    }
    let success = "hello";
    let event = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "tool_count":1,"tool_calls":[{"name":"bash","ok":true,"execution_ok":false,"ms":1,
            "result_full":success,"output_bytes":success.len()}],"tool_outcomes":outcomes,"llm_rounds":1});
    std::fs::write(&path, format!("{event}\n")).unwrap();
    assert_eq!(parse()["aggregates"]["observed_root_tool_failures"], 1);

    // A partial list retains its observed producer facts, without claiming
    // the missing records are known, for both successful and failed turns.
    let partial = json!({"name":"bash","ok":true,"ms":1,"result_preview":"{\"status\":",
        "output_bytes":100});
    let mut partial_event = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "tool_count":2,"tool_calls":[partial],"tool_outcomes":{"requested":2,"executed":2,
        "succeeded":2,"failed":0,"started_unknown":0,"unknown":0,"rejected":0,
        "reused":0,"suppressed":0,"deferred":0},"llm_rounds":1});
    for kind in ["turn", "turn_error"] {
        partial_event["type"] = json!(kind);
        std::fs::write(&path, format!("{partial_event}\n")).unwrap();
        let digest = parse();
        assert_eq!(digest["aggregates"]["root_tool_totals_complete"], false);
        assert_eq!(digest["aggregates"]["reported_root_fresh_tool_calls"], 1);
        assert_eq!(
            digest["aggregates"]["reported_root_tool_outcomes_unknown"],
            0
        );
    }

    // Server-owned turns can retain the complete ledger without retaining
    // per-call details. Keep ledger failures and observed failures separate.
    for kind in ["turn", "turn_error"] {
        for (succeeded, failed) in [(1, 0), (0, 1)] {
            let event = json!({"type":kind,"ts":"t1","session_id":sid,"turn":1,
                "tool_count":1,"tool_outcomes":{"requested":1,"executed":1,
                    "succeeded":succeeded,"failed":failed,"started_unknown":0,"unknown":0,
                    "rejected":0,"reused":0,"suppressed":0,"deferred":0}});
            std::fs::write(&path, format!("{event}\n")).unwrap();
            let digest = parse();
            assert_eq!(
                digest["aggregates"]["reported_root_ledger_failed_executions"],
                failed
            );
            assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 0);
            assert_eq!(digest["aggregates"]["root_tool_totals_complete"], false);
            let text = run_journal(
                root.path(),
                &["digest", sid, "--format", "text", "--focus", "all"],
            );
            assert!(text.status.success());
            assert!(String::from_utf8_lossy(&text.stdout).contains(&format!(
                "ledger_failed_executions={failed} observed_failures=0"
            )));
        }
    }
    let mut ledger_and_detail = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "tool_count":2,"tool_outcomes":{"requested":2,"executed":2,"succeeded":1,
            "failed":1,"started_unknown":0,"unknown":0,"rejected":0,"reused":0,
            "suppressed":0,"deferred":0},
        "tool_calls":[{"name":"bash","ok":false,"ms":1}]});
    std::fs::write(&path, format!("{ledger_and_detail}\n")).unwrap();
    let digest = parse();
    assert_eq!(
        digest["aggregates"]["reported_root_ledger_failed_executions"],
        1
    );
    assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 1);
    ledger_and_detail["tool_outcomes"]["failed"] = json!(0);
    ledger_and_detail["tool_outcomes"]["succeeded"] = json!(2);
    std::fs::write(&path, format!("{ledger_and_detail}\n")).unwrap();
    let contradictory = parse();
    assert_eq!(
        contradictory["aggregates"]["reported_root_ledger_failed_executions"],
        0
    );
    assert_eq!(
        contradictory["aggregates"]["observed_root_tool_failures"],
        1
    );
    assert_eq!(
        contradictory["aggregates"]["root_tool_counts_complete"],
        false
    );

    let failed_outcomes = json!({"requested":1,"executed":1,"succeeded":0,"failed":1,
        "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0});
    let event = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,"tool_count":1,
        "tool_calls":[{"name":"bash","ok":false,"ms":1,"error":"safety guard blocked command"}],
        "tool_outcomes":failed_outcomes,"llm_rounds":1});
    std::fs::write(&path, format!("{event}\n")).unwrap();
    let digest = parse();
    assert_eq!(digest["aggregates"]["safety_guard_blocks"], 1);
    assert_eq!(
        digest["failed_tool_calls"][0]["error_category"],
        "safety_guard"
    );
    assert_eq!(
        digest["failed_tool_calls"][0]["error_preview"],
        "safety guard blocked command"
    );

    let record =
        json!({"name":"bash","ok":true,"ms":1,"result_full":success,"output_bytes":success.len()});
    let mut first = json!({"type":"turn_error","ts":"t1","session_id":sid,"turn":1,
        "tool_count":1,"tool_calls":[record],"tool_outcomes":outcomes});
    let mut second = first.clone();
    second["type"] = json!("turn");
    second["turn"] = json!(2);
    second["ts"] = json!("t2");
    second["llm_rounds"] = json!(1);
    std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
    let missing = parse();
    assert!(missing["aggregates"]["avg_tool_calls_per_round"].is_null());
    assert!(missing["aggregates"]["avg_llm_rounds"].is_null());
    first["llm_rounds"] = json!(1);
    std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
    let known = parse();
    assert_eq!(known["aggregates"]["avg_tool_calls_per_round"], 1.0);
    assert_eq!(known["aggregates"]["avg_llm_rounds"], 1.0);

    first["llm_rounds"] = json!(0);
    std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
    let inconsistent = parse();
    assert!(inconsistent["aggregates"]["avg_tool_calls_per_round"].is_null());
    assert!(inconsistent["aggregates"]["avg_llm_rounds"].is_null());

    first["tool_count"] = json!(0);
    first["tool_calls"] = json!([]);
    first["tool_outcomes"] = json!({"requested":0,"executed":0,"succeeded":0,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0});
    std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
    let zero = parse();
    assert_eq!(zero["aggregates"]["avg_tool_calls_per_round"], 1.0);
    assert_eq!(zero["aggregates"]["avg_llm_rounds"], 0.5);

    // Matched round observations contradict a smaller terminal round count.
    let mut terminal = first.clone();
    terminal["metadata"] = json!({"run_id":"root-run"});
    let round = |number| {
        json!({"type":"llm_round","ts":format!("t{number}"),"session_id":sid,
        "turn":1,"round":number,"producer_scope":{"run_id":"root-run"}})
    };
    for (reported, observed) in [(0, 1), (1, 2), (2, 2)] {
        terminal["llm_rounds"] = json!(reported);
        let mut events = (0..observed)
            .map(|n| format!("{}\n", round(n)))
            .collect::<String>();
        events.push_str(&format!("{terminal}\n"));
        std::fs::write(&path, events).unwrap();
        let digest = parse();
        if reported < observed {
            assert!(digest["aggregates"]["avg_llm_rounds"].is_null());
            assert!(digest["aggregates"]["avg_tool_calls_per_round"].is_null());
        } else {
            assert_eq!(digest["aggregates"]["avg_llm_rounds"], 2.0);
            assert_eq!(digest["aggregates"]["avg_tool_calls_per_round"], 0.0);
        }
    }
    terminal["llm_rounds"] = json!(1);
    std::fs::write(&path, format!("{}\n{terminal}\n", round(2))).unwrap();
    let sparse = parse();
    assert!(sparse["aggregates"]["avg_llm_rounds"].is_null());
    assert!(sparse["aggregates"]["avg_tool_calls_per_round"].is_null());
}

#[test]
fn execution_counts_agree_across_commands_with_nonexecuted_requests() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "32a27bc8-485e-4444-a9a2-bc89db01dedb";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let parse = |args: &[&str]| {
        let output = run_journal(root.path(), args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"root-run"},"tool_calls_returned":5});
    let mut terminal = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
    "producer_scope":{"run_id":"root-run"},"llm_rounds":1,"tool_count":1,
    "tool_outcomes":{"requested":5,"executed":1,"succeeded":1,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":1,"reused":1,"suppressed":1,"deferred":1},
    "tool_calls":[
        {"name":"bash","ok":true,"ms":1,"disposition":"executed","result_preview":"hello"},
        {"name":"read_file","ok":false,"ms":0,"disposition":"rejected","error":"denied"},
        {"name":"read_file","ok":true,"ms":0,"disposition":"reused"},
        {"name":"read_file","ok":true,"ms":0,"disposition":"suppressed"},
        {"name":"read_file","ok":false,"ms":0,"disposition":"deferred","error":"not admitted"}
    ]});
    for kind in ["turn", "turn_error"] {
        terminal["type"] = json!(kind);
        std::fs::write(&path, format!("{round}\n{terminal}\n")).unwrap();
        let digest = parse(&["digest", sid, "--format", "json"]);
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(digest["aggregates"]["observed_root_tool_requests"], 5);
        assert_eq!(digest["aggregates"]["reported_root_tool_calls"], 1);
        assert_eq!(digest["aggregates"]["reported_root_fresh_tool_calls"], 1);
        assert_eq!(
            digest["aggregates"]["reported_root_noop_or_cached_tool_calls"],
            2
        );
        assert_eq!(digest["aggregates"]["observed_root_tool_failures"], 2);
        assert_eq!(digest["aggregates"]["root_tool_counts_complete"], true);
        assert_eq!(digest["aggregates"]["avg_tool_calls_per_round"], 1.0);
        assert_eq!(tree["root"]["tool_calls"], 1);
        assert_eq!(diff["a_tool_calls_executed"], 1);

        // A consistent outcome summary also carries the execution count when
        // the scalar was not retained.
        terminal.as_object_mut().unwrap().remove("tool_count");
        std::fs::write(&path, format!("{round}\n{terminal}\n")).unwrap();
        let digest = parse(&["digest", sid, "--format", "json"]);
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(digest["aggregates"]["reported_root_tool_calls"], 1);
        assert_eq!(tree["root"]["tool_calls"], 1);
        assert_eq!(diff["a_tool_calls_executed"], 1);
        terminal["tool_count"] = json!(1);
    }

    // Partial detail is still evidence: it must not be contradicted by a
    // smaller summary or scalar, even when other requested calls are missing.
    let identity = |id: &str| {
        json!({"user_id":"owner","session_id":sid,"run_id":"root-run",
        "turn_chain_id":"chain","invocation_id":id})
    };
    for (summary, calls) in [
        (
            json!({"requested":3,"executed":1,"succeeded":1,"failed":0,"started_unknown":0,
            "unknown":0,"rejected":2,"reused":0,"suppressed":0,"deferred":0}),
            json!([{"name":"bash","ok":true,"ms":1,"tool_call_id":"a","invocation_identity":identity("a")},
            {"name":"bash","ok":true,"ms":1,"tool_call_id":"b","invocation_identity":identity("b")}]),
        ),
        (
            json!({"requested":2,"executed":2,"succeeded":2,"failed":0,"started_unknown":0,
            "unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0}),
            json!([{"name":"bash","ok":true,"ms":1,"tool_call_id":"a","invocation_identity":identity("a")}]),
        ),
    ] {
        for kind in ["turn", "turn_error"] {
            let event = json!({"type":kind,"ts":"t","session_id":sid,"turn":1,
                "producer_scope":{"run_id":"root-run"},"llm_rounds":1,"tool_count":1,
                "tool_outcomes":summary,"tool_calls":calls});
            std::fs::write(&path, format!("{event}\n")).unwrap();
            let digest = parse(&["digest", sid, "--format", "json"]);
            let tree = parse(&["tree", sid, "--format", "json"]);
            let diff = parse(&["diff", sid, sid, "--format", "json"]);
            assert_eq!(digest["aggregates"]["root_tool_counts_complete"], false);
            assert!(digest["aggregates"]["avg_tool_calls_per_round"].is_null());
            assert_eq!(tree["root"]["tool_calls_complete"], false);
            assert_eq!(diff["a_tool_sequence_complete"], false);
        }
    }

    // A partial terminal can retain repeated observations of one invocation;
    // unidentified records are not proof of additional executions either.
    let call = |id: &str| {
        json!({"name":"bash","ok":true,"ms":1,"tool_call_id":id,
            "invocation_identity":identity(id)})
    };
    for (calls, expected) in [
        (json!([call("a"), call("a")]), 1),
        (json!([call("a"), call("b")]), 2),
        (json!([call("a"), {"name":"bash","ok":true,"ms":1}]), 1),
    ] {
        for kind in ["turn", "turn_error"] {
            let event = json!({"type":kind,"ts":"t","session_id":sid,"turn":1,
                "producer_scope":{"run_id":"root-run"},"tool_calls":calls});
            std::fs::write(&path, format!("{event}\n")).unwrap();
            let digest = parse(&["digest", sid, "--format", "json"]);
            let tree = parse(&["tree", sid, "--format", "json"]);
            let diff = parse(&["diff", sid, sid, "--format", "json"]);
            assert_eq!(digest["aggregates"]["reported_root_tool_calls"], expected);
            assert_eq!(tree["root"]["tool_calls"], expected);
            assert_eq!(tree["root"]["tool_calls_complete"], false);
            assert_eq!(diff["a_tool_calls_executed"], expected);
            assert_eq!(diff["a_tool_sequence_complete"], false);
        }
    }

    // A replay cannot simultaneously establish contradictory outcomes for
    // one invocation, even when the raw record counts fit the summary.
    let mut failed_call = call("a");
    failed_call["ok"] = json!(false);
    let event = json!({"type":"turn","ts":"t","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"root-run"},"tool_count":2,
        "tool_outcomes":{"requested":2,"executed":2,"succeeded":1,"failed":1,
            "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[call("a"),failed_call]});
    std::fs::write(&path, format!("{event}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 1);
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_calls_executed"], 1);
    assert_eq!(diff["a_tool_sequence_complete"], false);

    // Contradictory raw reports are not confirmed execution counts.
    let event = json!({"type":"turn","ts":"t","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"root-run"},"tool_count":1000,
        "tool_outcomes":{"requested":1,"executed":1,"succeeded":1,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[call("a")]});
    std::fs::write(&path, format!("{event}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 1);
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_calls_executed"], 1);
    assert_eq!(diff["a_tool_sequence_complete"], false);
    let output = run_journal(root.path(), &["diff", sid, sid]);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("confirmed executed calls: 1;"));

    // When a journal source is incomplete, reports from different attempts
    // need not be disjoint even if each retained terminal is consistent.
    let terminal = |turn, count| {
        json!({"type":"turn","ts":format!("t{turn}"),"session_id":sid,
        "turn":turn,"producer_scope":{"run_id":format!("run-{turn}")},"tool_count":count,
        "tool_outcomes":{"requested":count,"executed":count,"succeeded":count,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0}})
    };
    std::fs::write(
        &path,
        format!("{}\n{}\nnot json\n", terminal(1, 2), terminal(2, 3)),
    )
    .unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 3);
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_calls_executed"], 3);
    assert_eq!(diff["a_tool_sequence_complete"], false);
}

#[test]
fn tree_does_not_attribute_rounds_to_conflicting_or_ambiguous_subruns() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "3fc9c724-2b34-499a-b525-f73b0f668eff";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let terminal = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"root"},"tool_count":0});
    let started = json!({"type":"delegation_sub_run_started","ts":"t2","session_id":sid,
        "metadata":{"delegation_id":"d","sub_run_id":"child","parent_run_id":"root",
            "agent_id":"agent-a","depth":1}});
    let mut round = json!({"type":"llm_round","ts":"t3","session_id":sid,"round":0,
        "producer_scope":{"run_id":"child","parent_run_id":"root","agent_id":"agent-a",
            "local_turn":1},"tokens_in":99,"tokens_out":10});
    let tree = |events: &[Value]| {
        std::fs::write(
            &path,
            events
                .iter()
                .map(|event| format!("{event}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let output = run_journal(root.path(), &["tree", sid, "--format", "json"]);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let base = [terminal.clone(), started.clone(), round.clone()];
    assert_eq!(
        tree(&base)["root"]["children"][0]["observed_round_input"],
        99
    );
    round["producer_scope"]
        .as_object_mut()
        .unwrap()
        .remove("parent_run_id");
    assert_eq!(
        tree(&[terminal.clone(), started.clone(), round.clone()])["root"]["children"][0]["observed_round_input"],
        99
    );
    round["producer_scope"]["parent_run_id"] = json!("");
    assert_eq!(
        tree(&[terminal.clone(), started.clone(), round.clone()])["root"]["children"][0]["observed_round_input"],
        99
    );
    round["producer_scope"]
        .as_object_mut()
        .unwrap()
        .remove("parent_run_id");
    for (field, wrong) in [("parent_run_id", "other-parent"), ("agent_id", "agent-b")] {
        let mut conflict = round.clone();
        conflict["producer_scope"][field] = json!(wrong);
        let result = tree(&[terminal.clone(), started.clone(), conflict]);
        assert!(result["root"]["children"][0]["observed_round_input"].is_null());
        assert!(result["skipped_events"].as_u64().unwrap() > 0);
    }
    let mut other = round.clone();
    other["round"] = json!(1);
    other["producer_scope"]["local_turn"] = json!(2);
    other["producer_scope"]["agent_id"] = json!("agent-b");
    other["producer_scope"]["parent_run_id"] = json!("other-parent");
    std::fs::write(&path, format!("{terminal}\n{started}\n{round}\n{other}\n")).unwrap();
    let output = run_journal(
        root.path(),
        &["digest", sid, "--format", "json", "--focus", "all"],
    );
    assert!(output.status.success(), "{output:?}");
    let digest: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = digest["subruns"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row["agent_id"] == "agent-a"));
    assert!(rows.iter().any(|row| row["agent_id"] == "agent-b"));
    let mut duplicate = started.clone();
    duplicate["ts"] = json!("t4");
    duplicate["metadata"]["agent_id"] = json!("agent-b");
    let completed = json!({"type":"delegation_sub_run_completed","ts":"t3.5","session_id":sid,
        "metadata":{"delegation_id":"d","sub_run_id":"child","agent_id":"agent-a",
            "status":"completed"}});
    let result = tree(&[terminal, started, round, completed, duplicate]);
    assert!(result["root"]["unattached"][0]["observed_round_input"].is_null());
    assert!(result["root"]["unattached"][0]["ended_at"].is_null());
}

#[test]
fn conflicting_invocation_dispositions_remain_unconfirmed() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "7a3dbf09-4b60-44aa-9d46-aa04fc4cae14";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let call = |id: &str, disposition: &str| {
        json!({
            "name":"bash", "ok": disposition == "executed", "ms":1,
            "tool_call_id":id, "disposition":disposition,
            "invocation_identity":{"user_id":"owner","session_id":sid,"run_id":"run",
                "turn_chain_id":"chain","invocation_id":id}
        })
    };
    let summary = json!({"requested":1,"executed":0,"succeeded":0,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":1,"reused":0,"suppressed":0,"deferred":0});
    let parse = |args: &[&str]| {
        let output = run_journal(root.path(), args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    for kind in ["turn", "turn_error"] {
        let event = json!({"type":kind,"ts":"t","session_id":sid,"turn":1,
            "producer_scope":{"run_id":"run"},"tool_count":0,"tool_outcomes":summary,
            "tool_calls":[call("a","executed"),call("a","rejected")]});
        std::fs::write(&path, format!("{event}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], 0, "{kind}: {tree}");
        assert_eq!(tree["root"]["tool_calls_complete"], false);
        assert_eq!(diff["a_tool_calls_executed"], 0, "{kind}: {diff}");
        assert_eq!(diff["a_tool_sequence_complete"], false);
    }

    // The same conflict can be split across a retained round and terminal.
    let rejected = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"run"},"tool_calls":[call("a","rejected")]});
    let executed = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tool_calls":[call("a","executed")]});
    std::fs::write(&path, format!("{rejected}\n{executed}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");
    assert_eq!(diff["a_tool_sequence_complete"], false);

    // An anonymous row may be another projection of the disputed call; it
    // cannot by itself promote that call to a confirmed execution.
    let anonymous = json!({"name":"bash","ok":true,"ms":1,"disposition":"executed"});
    let ambiguous = json!({"type":"turn","ts":"t","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":0,"tool_outcomes":summary,
        "tool_calls":[call("a","executed"),call("a","rejected"),anonymous]});
    std::fs::write(&path, format!("{ambiguous}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");

    // An explicitly different producer run supplies independent anonymous
    // execution evidence without inventing an invocation identity.
    let independent_round = json!({"type":"llm_round","ts":"t2","session_id":sid,"turn":2,"round":0,
        "producer_scope":{"run_id":"other-run"},
        "tool_calls":[{"name":"read_file","ok":true,"ms":1,"disposition":"executed"}]});
    std::fs::write(&path, format!("{ambiguous}\n{independent_round}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 1, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 1, "{diff}");

    // The actual child callback producer writes a scoped sync marker.
    // A different run or tool is independent of the dispute.
    for (run, expected) in [(Some("child-run"), 1), (Some("run"), 1), (None, 0)] {
        let mut marker = json!({"type":"sync_marker","ts":"t2","session_id":sid,
            "metadata":{"source":"child_agent"},
            "tool_calls":[{"name":"read_file","ok":true,"ms":1,"disposition":"executed"}]});
        if let Some(run) = run {
            marker["producer_scope"] = json!({"run_id":run});
        }
        std::fs::write(&path, format!("{ambiguous}\n{marker}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], expected, "{tree}");
        assert_eq!(diff["a_tool_calls_executed"], expected, "{diff}");
    }

    // A real scoped child callback can contain both an execution and a
    // rejection. Distinct producer request IDs establish distinct requests.
    for (rejected_id, expected) in [(Some("other"), 1), (Some("same"), 0), (None, 0)] {
        let executed = json!({"name":"read_file","ok":true,"ms":1,
            "disposition":"executed","tool_call_id":"same"});
        let mut rejected = json!({"name":"read_file","ok":false,"ms":0,"disposition":"rejected"});
        if let Some(id) = rejected_id {
            rejected["tool_call_id"] = json!(id);
        }
        let marker = json!({"type":"sync_marker","ts":"t2","session_id":sid,
            "producer_scope":{"run_id":"child-run"},"metadata":{"source":"child_agent"},
            "tool_calls":[executed,rejected]});
        std::fs::write(&path, format!("{ambiguous}\n{marker}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], expected, "{tree}");
        assert_eq!(diff["a_tool_calls_executed"], expected, "{diff}");
    }

    // Error producers can omit parent/agent fields even for a child.
    // Count a nonterminal failure as root only when its run and turn match.
    let root_terminal = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":0,
        "tool_outcomes":{"requested":0,"executed":0,"succeeded":0,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[]});
    for (run, expected) in [(Some("run"), 1), (Some("child-run"), 0), (None, 0)] {
        let mut failure = json!({"type":"tool_call_error","ts":"t1","session_id":sid,"turn":1,
            "error":"failed","tool_calls":[{"name":"bash","ok":false,"ms":1,"disposition":"executed"}]});
        if let Some(run) = run {
            failure["producer_scope"] = json!({"run_id":run});
        }
        std::fs::write(&path, format!("{failure}\n{root_terminal}\n")).unwrap();
        let digest = parse(&["digest", sid, "--format", "json"]);
        assert_eq!(
            digest["aggregates"]["observed_root_tool_failures"], expected,
            "{digest}"
        );
    }

    // A summary-only aggregate cannot be assumed independent when its
    // serialized shape does not distinguish a ledger from derived details.
    let conflicted_round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"run"},"tool_calls":[call("a","executed"),call("a","rejected")]});
    let summary_only = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":5,"tool_outcomes":{
            "requested":6,"executed":5,"succeeded":5,"failed":0,"started_unknown":0,
            "unknown":0,"rejected":1,"reused":0,"suppressed":0,"deferred":0}});
    std::fs::write(&path, format!("{conflicted_round}\n{summary_only}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_sequence_complete"], false);
    let digest = parse(&["digest", sid, "--format", "json"]);
    assert_eq!(
        digest["aggregates"]["reported_root_tool_calls"], 5,
        "{digest}"
    );
    assert_eq!(digest["aggregates"]["root_tool_counts_complete"], false);

    // A later terminal that mentions the same call does not make an
    // earlier same-attempt summary independent of the conflict.
    let later = json!({"type":"turn","ts":"t3","session_id":sid,"turn":2,
        "producer_scope":{"run_id":"other-run"},"tool_count":1,
        "tool_calls":[call("a","executed")]});
    std::fs::write(
        &path,
        format!("{conflicted_round}\n{summary_only}\n{later}\n"),
    )
    .unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");

    // An earlier turn cannot certify a later attempt's anonymous terminal.
    let one = json!({"requested":1,"executed":1,"succeeded":1,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0});
    let first = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tool_outcomes":one,
        "tool_calls":[call("a","executed")]});
    let later_round = json!({"type":"llm_round","ts":"t2","session_id":sid,"turn":2,"round":0,
        "producer_scope":{"run_id":"run"},"tool_calls":[call("a","deferred")]});
    let later_terminal = json!({"type":"turn","ts":"t3","session_id":sid,"turn":2,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tool_outcomes":one,
        "tool_calls":[{"name":"bash","ok":true,"ms":1,"disposition":"executed"}]});
    std::fs::write(&path, format!("{first}\n{later_round}\n{later_terminal}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    let digest = parse(&["digest", sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 1, "{tree}");
    assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 1, "{diff}");
    assert_eq!(
        digest["aggregates"]["root_tool_counts_complete"], false,
        "{digest}"
    );

    // A later authoritative terminal can refine a nonterminal unknown or
    // deferred observation without making its execution count unknowable.
    for disposition in ["unknown", "deferred"] {
        let mut provisional = call("a", disposition);
        if disposition == "deferred" {
            provisional["error"] = json!("worker busy");
        }
        let round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
            "producer_scope":{"run_id":"run"},"tool_calls":[provisional]});
        let terminal = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
            "producer_scope":{"run_id":"run"},"llm_rounds":1,"tool_count":1,
            "tool_outcomes":{"requested":1,"executed":1,"succeeded":1,"failed":0,
                "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0},
            "tool_calls":[call("a","executed")]});
        std::fs::write(&path, format!("{round}\n{terminal}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
        assert_eq!(tree["root"]["tool_calls"], 1, "{disposition}: {tree}");
        assert_eq!(
            tree["root"]["tool_calls_complete"], true,
            "{disposition}: {tree}"
        );
        assert_eq!(
            digest["aggregates"]["root_tool_counts_complete"], true,
            "{disposition}: {digest}"
        );
        assert_eq!(
            digest["aggregates"]["root_tool_totals_complete"], false,
            "{disposition}: {digest}"
        );
        let observed_failures = u8::from(disposition == "deferred");
        assert_eq!(
            digest["aggregates"]["observed_root_tool_failures"], observed_failures,
            "{disposition}: {digest}"
        );
        let output = run_journal(
            root.path(),
            &["digest", sid, "--format", "text", "--focus", "summary"],
        );
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("tool_failures=?"), "{disposition}: {text}");
        if disposition == "deferred" {
            assert!(text.contains("observed_failures=1"), "{text}");
            assert_eq!(
                digest["failed_tool_calls"][0]["error_preview"], "worker busy",
                "{digest}"
            );
            // Replayed projections of the same invocation remain one failure.
            std::fs::write(&path, format!("{round}\n{round}\n{terminal}\n")).unwrap();
            let repeated = parse(&["digest", sid, "--format", "json"]);
            assert_eq!(
                repeated["aggregates"]["observed_root_tool_failures"], 1,
                "{repeated}"
            );
        }

        // A different invocation cannot refine this observation.
        let mut other_terminal = terminal;
        other_terminal["tool_calls"] = json!([call("b", "executed")]);
        std::fs::write(&path, format!("{round}\n{other_terminal}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let digest = parse(&["digest", sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");
        assert_eq!(
            digest["aggregates"]["root_tool_counts_complete"], false,
            "{digest}"
        );
    }

    // Round replay deduplication must retain different tool facts without
    // turning the replay into a second model round.
    let round_exec = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"run"},"tool_calls":[call("a","executed")]});
    let mut round_rejected = round_exec.clone();
    round_rejected["tool_calls"] = json!([call("a", "rejected")]);
    let settled = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tool_calls":[call("a","executed")]});
    std::fs::write(
        &path,
        format!("{round_exec}\n{round_rejected}\n{settled}\n"),
    )
    .unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");

    std::fs::write(&path, format!("{round_exec}\n{round_exec}\n{settled}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 1, "{tree}");
    assert_eq!(tree["root"]["tool_calls_complete"], true, "{tree}");

    // Conflicting telemetry cannot hide either tool disposition.
    let mut conflicting_round_a = round_exec.clone();
    conflicting_round_a["tokens_in"] = json!(10);
    let mut conflicting_round_b = round_rejected.clone();
    conflicting_round_b["tokens_in"] = json!(20);
    conflicting_round_b["tool_calls"][0]["error"] = json!("denied during round");
    std::fs::write(
        &path,
        format!("{conflicting_round_a}\n{conflicting_round_b}\n{settled}\n"),
    )
    .unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");
    assert!(digest["turns"][0]["llm_rounds"].is_null(), "{digest}");
    assert_eq!(
        digest["aggregates"]["observed_root_tool_failures"], 1,
        "{digest}"
    );
    assert_eq!(
        digest["failed_tool_calls"][0]["error_preview"], "denied during round",
        "{digest}"
    );

    let mut failed_a = call("a", "rejected");
    failed_a["error"] = json!("denied a");
    let mut failed_b = call("b", "rejected");
    failed_b["error"] = json!("denied b");
    let mut round_a = round_exec.clone();
    round_a["tool_calls"] = json!([failed_a]);
    let mut round_b = round_exec.clone();
    round_b["tool_calls"] = json!([failed_b]);
    let both_rejected = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":0,
        "tool_outcomes":{"requested":2,"executed":0,"succeeded":0,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":2,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[failed_a,failed_b]});
    std::fs::write(&path, format!("{round_a}\n{round_b}\n{both_rejected}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(
        digest["aggregates"]["observed_root_tool_failures"], 2,
        "{digest}"
    );
    assert_eq!(
        digest["failed_tool_calls"].as_array().unwrap().len(),
        2,
        "{digest}"
    );

    // Child tool evidence survives conflicting telemetry without creating
    // two child rounds. An exact replay is also one round.
    let mut child_exec = call("a", "executed");
    child_exec["invocation_identity"]["run_id"] = json!("child");
    let mut child_reject = call("a", "rejected");
    child_reject["invocation_identity"]["run_id"] = json!("child");
    let child_a = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "tokens_in":10,"producer_scope":{"run_id":"child","parent_run_id":"run","agent_id":"worker"},
        "tool_calls":[child_exec]});
    let mut child_b = child_a.clone();
    child_b["tokens_in"] = json!(20);
    child_b["tool_calls"] = json!([child_reject]);
    for second in [&child_a, &child_b] {
        std::fs::write(&path, format!("{child_a}\n{second}\n{settled}\n")).unwrap();
        let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
        assert_eq!(
            digest["subruns"][0]["llm_round_details"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{digest}"
        );
        let text = run_journal(
            root.path(),
            &["digest", sid, "--format", "text", "--focus", "all"],
        );
        assert!(text.status.success(), "{text:?}");
        assert!(String::from_utf8(text.stdout).unwrap().contains("1 rounds"));
    }

    // A scope-invalid identity is ambiguous, not an anonymous execution.
    for (valid, invalid, expected) in [
        ("rejected", "executed", 0),
        ("executed", "rejected", 0),
        ("executed", "executed", 1),
    ] {
        let root_terminal = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
            "producer_scope":{"run_id":"run"},"tool_count":u8::from(valid == "executed"),
            "tool_calls":[call("a",valid)]});
        let invalid_round = json!({"type":"llm_round","ts":"t2","session_id":sid,"turn":2,"round":0,
            "producer_scope":{"run_id":"other-run"},"tool_calls":[call("a",invalid)]});
        std::fs::write(&path, format!("{root_terminal}\n{invalid_round}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], expected, "{tree}");
        assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");
        assert_eq!(diff["a_tool_calls_executed"], expected, "{diff}");
        if valid == "rejected" {
            let independent = json!({"type":"llm_round","ts":"t3","session_id":sid,"turn":3,"round":0,
                "producer_scope":{"run_id":"third-run"},
                "tool_calls":[{"name":"read_file","ok":true,"ms":1,"disposition":"executed"}]});
            std::fs::write(
                &path,
                format!("{root_terminal}\n{invalid_round}\n{independent}\n"),
            )
            .unwrap();
            let tree = parse(&["tree", sid, "--format", "json"]);
            assert_eq!(tree["root"]["tool_calls"], 1, "{tree}");
        }
    }

    // An invalid identity inside the terminal must not be certified by
    // digest while tree and diff treat it as ambiguous.
    let mut invalid_call = call("a", "executed");
    invalid_call["tool_call_id"] = json!("different-id");
    let invalid_terminal = json!({"type":"turn","ts":"t","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,
        "tool_outcomes":{"requested":1,"executed":1,"succeeded":1,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[invalid_call]});
    std::fs::write(&path, format!("{invalid_terminal}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    let digest = parse(&["digest", sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");
    assert_eq!(diff["a_tool_sequence_complete"], false, "{diff}");
    assert_eq!(
        digest["aggregates"]["root_tool_counts_complete"], false,
        "{digest}"
    );

    // Opposite invalid projections are still potentially the same call.
    let mut invalid_executed = call("a", "executed");
    invalid_executed["tool_call_id"] = json!("different-id");
    let mut invalid_rejected = call("a", "rejected");
    invalid_rejected["tool_call_id"] = json!("different-id");
    let contradictory = json!({"type":"turn","ts":"t","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":0,"tool_outcomes":summary,
        "tool_calls":[invalid_executed,invalid_rejected]});
    std::fs::write(&path, format!("{contradictory}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");

    // An unidentified execution cannot be proven distinct from a rejection
    // in the same run; a different producer run is independently observed.
    let rejected_terminal = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":0,"tool_outcomes":summary,
        "tool_calls":[call("a","rejected")]});
    for (run, name, expected) in [
        ("run", "bash", 0),
        ("other-run", "bash", 1),
        ("run", "read_file", 1),
    ] {
        let observation = json!({"type":"llm_round","ts":"t2","session_id":sid,"turn":2,"round":0,
            "producer_scope":{"run_id":run},
            "tool_calls":[{"name":name,"ok":true,"ms":1,"disposition":"executed"}]});
        std::fs::write(&path, format!("{rejected_terminal}\n{observation}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], expected, "{tree}");
        assert_eq!(diff["a_tool_calls_executed"], expected, "{diff}");
    }

    // A derived terminal summary cannot make anonymous, conflicting
    // projections into a confirmed execution or complete sequence.
    let anonymous_reject = json!({"name":"bash","ok":false,"ms":0,
        "disposition":"rejected","tool_call_id":"same"});
    let anonymous_execute = json!({"name":"bash","ok":true,"ms":1,
        "disposition":"executed","tool_call_id":"same"});
    let one_executed = json!({"requested":1,"executed":1,"succeeded":1,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0});
    let one_each = json!({"requested":2,"executed":1,"succeeded":1,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":1,"reused":0,"suppressed":0,"deferred":0});
    let rejection_round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"run"},"tool_calls":[anonymous_reject]});
    let terminal_exec = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tool_outcomes":one_executed,
        "tool_calls":[anonymous_execute]});
    let terminal_both = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tool_outcomes":one_each,
        "tool_calls":[anonymous_execute,anonymous_reject]});
    for events in [
        format!("{rejection_round}\n{terminal_exec}\n"),
        format!("{terminal_both}\n"),
    ] {
        std::fs::write(&path, events).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        let digest = parse(&["digest", sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
        assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");
        assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");
        assert_eq!(diff["a_tool_sequence_complete"], false, "{diff}");
        assert_eq!(
            digest["aggregates"]["reported_root_tool_calls"], 1,
            "{digest}"
        );
        assert_eq!(
            digest["aggregates"]["root_tool_counts_complete"], false,
            "{digest}"
        );
    }
    // Conflicting anonymous facts confined to a round also taint a
    // same-attempt summary that has no retained call details.
    let conflicting_round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"run"},"tool_calls":[anonymous_execute,anonymous_reject]});
    let summary_only = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tool_outcomes":one_executed});
    std::fs::write(&path, format!("{conflicting_round}\n{summary_only}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    let digest = parse(&["digest", sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");
    assert_eq!(diff["a_tool_sequence_complete"], false, "{diff}");
    assert_eq!(
        digest["aggregates"]["reported_root_tool_calls"], 1,
        "{digest}"
    );
    assert_eq!(
        digest["aggregates"]["root_tool_counts_complete"], false,
        "{digest}"
    );

    let mut different = terminal_exec.clone();
    different["tool_calls"][0]["tool_call_id"] = json!("different");
    std::fs::write(&path, format!("{rejection_round}\n{different}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 1, "{tree}");
    assert_eq!(tree["root"]["tool_calls_complete"], false, "{tree}");

    // A terminal may carry a previous run's invocation. Without a
    // validated identity, the terminal's own run does not establish scope.
    let cross_terminal = |turn: u32, run: &str, disposition: &str, valid: bool, id: &str| {
        let mut record = call(id, disposition);
        if valid {
            record["invocation_identity"]["run_id"] = json!(run);
        } else {
            record
                .as_object_mut()
                .unwrap()
                .remove("invocation_identity");
        }
        let executed = u8::from(disposition == "executed");
        json!({"type":"turn","ts":format!("t{turn}"),"session_id":sid,"turn":turn,
            "producer_scope":{"run_id":run},"tool_count":executed,
            "tool_outcomes":{"requested":1,"executed":executed,"succeeded":executed,"failed":0,
                "started_unknown":0,"unknown":0,"rejected":1-executed,"reused":0,"suppressed":0,"deferred":0},
            "tool_calls":[record]})
    };
    for (first, second, expected, complete) in [
        (
            cross_terminal(1, "run", "executed", true, "a"),
            cross_terminal(2, "other-run", "rejected", false, "a"),
            0,
            false,
        ),
        (
            cross_terminal(1, "run", "rejected", true, "a"),
            cross_terminal(2, "other-run", "executed", false, "a"),
            0,
            false,
        ),
        (
            cross_terminal(1, "run", "executed", true, "a"),
            cross_terminal(2, "other-run", "rejected", true, "a"),
            1,
            true,
        ),
        (
            cross_terminal(1, "run", "executed", true, "a"),
            cross_terminal(2, "other-run", "rejected", false, "b"),
            1,
            true,
        ),
    ] {
        std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        let digest = parse(&["digest", sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], expected, "{tree}");
        assert_eq!(tree["root"]["tool_calls_complete"], complete, "{tree}");
        assert_eq!(diff["a_tool_calls_executed"], expected, "{diff}");
        assert_eq!(diff["a_tool_sequence_complete"], complete, "{diff}");
        assert_eq!(
            digest["aggregates"]["root_tool_counts_complete"], complete,
            "{digest}"
        );
    }

    // A summary-only rejection has no producer identity with which to
    // establish that an unidentified execution is a different request.
    let summary_rejection = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":0,"tool_outcomes":summary});
    let unidentified = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,"round":0,
        "producer_scope":{"run_id":"run"},
        "tool_calls":[{"name":"bash","ok":true,"ms":1,"disposition":"executed"}]});
    std::fs::write(&path, format!("{unidentified}\n{summary_rejection}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 0, "{tree}");
    assert_eq!(diff["a_tool_calls_executed"], 0, "{diff}");

    // Replayed safety-guard failures share one observation and one detail.
    let mut guard = call("a", "executed");
    guard["ok"] = json!(false);
    guard["error"] = json!("safety guard blocked command");
    let failed_one = json!({"requested":1,"executed":1,"succeeded":0,"failed":1,
        "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0});
    let first_guard = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tool_outcomes":failed_one,
        "tool_calls":[guard]});
    let mut second_guard = first_guard.clone();
    second_guard["turn"] = json!(2);
    second_guard["ts"] = json!("t2");
    std::fs::write(&path, format!("{first_guard}\n{second_guard}\n")).unwrap();
    let digest = parse(&["digest", sid, "--format", "json", "--focus", "all"]);
    assert_eq!(
        digest["aggregates"]["observed_root_tool_failures"], 1,
        "{digest}"
    );
    assert_eq!(digest["aggregates"]["safety_guard_blocks"], 1, "{digest}");
    assert_eq!(
        digest["failed_tool_calls"].as_array().unwrap().len(),
        1,
        "{digest}"
    );

    // A conflict must not erase an unrelated, independently retained
    // terminal aggregate or an unopposed invocation.
    let conflict = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":0,"tool_outcomes":summary,
        "tool_calls":[call("a","executed"),call("a","rejected"),call("b","executed")]});
    let independent = json!({"type":"turn","ts":"t2","session_id":sid,"turn":2,
        "producer_scope":{"run_id":"other-run"},"tool_count":3});
    std::fs::write(&path, format!("{conflict}\n{independent}\n")).unwrap();
    let tree = parse(&["tree", sid, "--format", "json"]);
    let diff = parse(&["diff", sid, sid, "--format", "json"]);
    assert_eq!(tree["root"]["tool_calls"], 3, "{tree}");
    assert_eq!(tree["root"]["tool_calls_complete"], false);
    assert_eq!(diff["a_tool_calls_executed"], 3, "{diff}");
    assert_eq!(diff["a_tool_sequence_complete"], false);

    // Replays are one execution; distinct identities are distinct. Unknown
    // and deferred observations can be refined by later execution evidence.
    for (calls, expected) in [
        (json!([call("a", "executed"), call("a", "executed")]), 1),
        (json!([call("a", "executed"), call("b", "executed")]), 2),
        (json!([call("a", "unknown"), call("a", "executed")]), 1),
        (json!([call("a", "deferred"), call("a", "executed")]), 1),
    ] {
        let event = json!({"type":"turn","ts":"t","session_id":sid,"turn":1,
            "producer_scope":{"run_id":"run"},"tool_calls":calls});
        std::fs::write(&path, format!("{event}\n")).unwrap();
        let tree = parse(&["tree", sid, "--format", "json"]);
        let diff = parse(&["diff", sid, sid, "--format", "json"]);
        assert_eq!(tree["root"]["tool_calls"], expected, "{event}: {tree}");
        assert_eq!(diff["a_tool_calls_executed"], expected, "{event}: {diff}");
    }
}

#[test]
fn partial_rejected_failures_remain_visible_in_digest_summary() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "e95ac7eb-150b-4478-9be8-59375da2ac51";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut event = json!({"type":"turn","ts":"t","session_id":sid,"turn":1,
        "tool_count":0,"tool_outcomes":{"requested":2,"executed":0,"succeeded":0,"failed":0,
        "started_unknown":0,"unknown":0,"rejected":2,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[{"name":"bash","ok":false,"ms":0,"disposition":"rejected","error":"denied"}]});
    for (calls, expected) in [(event["tool_calls"].clone(), 1), (json!([]), 0)] {
        event["tool_calls"] = calls;
        std::fs::write(&path, format!("{event}\n")).unwrap();
        let json = run_journal(
            root.path(),
            &["digest", sid, "--format", "json", "--focus", "summary"],
        );
        assert!(json.status.success(), "{json:?}");
        let digest: Value = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(
            digest["aggregates"]["observed_root_tool_failures"],
            expected
        );
        assert_eq!(digest["aggregates"]["root_tool_totals_complete"], false);
        let text = run_journal(
            root.path(),
            &["digest", sid, "--format", "text", "--focus", "summary"],
        );
        assert!(text.status.success(), "{text:?}");
        let text = String::from_utf8(text.stdout).unwrap();
        assert!(text.contains("tool_failures=?"), "{text}");
        assert_eq!(
            text.contains("observed_failures=1"),
            expected == 1,
            "{text}"
        );
    }
}

#[test]
fn consistent_mixed_terminal_keeps_its_execution_lower_bound() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "50f21eee-5b76-4c77-9991-8d0691e08961";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mixed = json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run-1"},"tool_count":1,
        "tool_outcomes":{"requested":2,"executed":1,"succeeded":1,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":1,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[{"name":"bash","ok":true,"ms":1,"disposition":"executed",
            "tool_call_id":"call-1","invocation_identity":{"user_id":"owner","session_id":sid,
                "run_id":"run-1","turn_chain_id":"chain","invocation_id":"call-1"}}]});
    let later = json!({"type":"turn","ts":"t2","session_id":sid,"turn":2,
        "producer_scope":{"run_id":"run-2"},"tool_count":3,
        "tool_outcomes":{"requested":3,"executed":3,"succeeded":3,"failed":0,
            "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0},
        "tool_calls":[{"name":"bash","ok":true,"ms":1,"disposition":"executed",
            "tool_call_id":"call-2","invocation_identity":{"user_id":"owner","session_id":sid,
                "run_id":"run-2","turn_chain_id":"chain","invocation_id":"call-2"}}]});
    let child_call = json!({"name":"bash","ok":true,"ms":1,"disposition":"executed",
        "tool_call_id":"child-call","invocation_identity":{"user_id":"owner","session_id":sid,
            "run_id":"child-run","turn_chain_id":"child-chain","invocation_id":"child-call"}});
    let child = json!({"type":"sync_marker","ts":"t0","session_id":sid,
        "producer_scope":{"run_id":"child-run"},"metadata":{"source":"child_agent"},
        "tool_calls":[child_call]});
    let mut overlap = mixed.clone();
    overlap["tool_calls"] = json!([child["tool_calls"][0]]);
    let mut child_rejection = child.clone();
    child_rejection["tool_calls"][0]["disposition"] = json!("rejected");
    child_rejection["tool_calls"][0]["ok"] = json!(false);
    child_rejection["tool_calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("invocation_identity");
    let mut zero = mixed.clone();
    zero["tool_count"] = json!(0);
    zero["tool_outcomes"]["requested"] = json!(1);
    zero["tool_outcomes"]["executed"] = json!(0);
    zero["tool_outcomes"]["succeeded"] = json!(0);
    zero["tool_calls"] = json!([]);
    let mut other_session = child.clone();
    other_session["session_id"] = json!("different-session");
    other_session["tool_calls"][0]["invocation_identity"]["session_id"] =
        json!("different-session");
    for (journal, lower_bound) in [
        (format!("{mixed}\n"), 1),
        (format!("{mixed}\n{later}\n"), 3),
        (format!("{mixed}\n{child}\n"), 2),
        (format!("{mixed}\n{child}\n{child}\n"), 2),
        (format!("{mixed}\n{child}\n{child_rejection}\n"), 1),
        (format!("{zero}\n{child}\n{child_rejection}\n"), 0),
        (format!("{overlap}\n{child}\n"), 1),
    ] {
        std::fs::write(&path, journal).unwrap();
        for args in [
            vec!["tree", sid, "--format", "json"],
            vec!["diff", sid, sid, "--format", "json"],
        ] {
            let output = run_journal(root.path(), &args);
            assert!(output.status.success(), "{args:?}: {output:?}");
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            let count = if args[0] == "tree" {
                &value["root"]["tool_calls"]
            } else {
                &value["a_tool_calls_executed"]
            };
            assert_eq!(count, lower_bound, "{args:?}: {value}");
        }
    }
    std::fs::write(&path, format!("{mixed}\n{other_session}\n")).unwrap();
    for args in [
        vec!["tree", sid, "--format", "json"],
        vec!["diff", sid, sid, "--format", "json"],
    ] {
        let output = run_journal(root.path(), &args);
        assert!(
            !output.status.success(),
            "cross-session journal must fail closed: {output:?}"
        );
    }
}

#[test]
fn conflicting_terminal_summary_omits_per_call_payloads() {
    let root = tempfile::tempdir().unwrap();
    let _journals = JournalDirGuard::new(root.path().join("state/sessions"));
    let sid = "1a75a089-2e4b-4ecd-a685-570e9b76f634";
    let owner = OwnerScope::user(local_profile_owner_id("default", None).unwrap()).unwrap();
    let path = journal_file_path_for_owner(&owner, sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let calls: Vec<Value> = (0..128)
        .map(|n| {
            json!({"name":"bash","ok":false,"ms":1,
        "disposition":"executed","tool_call_id":format!("call-{n}"),
        "error":"permission denied", "args_preview":"SECRET_MARKER tool arguments"})
        })
        .collect();
    let first = json!({"type":"turn_error","ts":"t1","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"error":"first terminal","tool_calls":calls});
    let mut second = first.clone();
    second["error"] = json!("second terminal");
    let round = json!({"type":"llm_round","ts":"t0","session_id":sid,"turn":1,
        "round":0,"producer_scope":{"run_id":"run"},"tool_calls":first["tool_calls"]});
    let errors = first["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| {
            json!({"type":"tool_call_error","ts":"t0","session_id":sid,"turn":1,
            "producer_scope":{"run_id":"run"},"error":call["error"],"tool_calls":[call]})
        })
        .map(|event| format!("{event}\n"))
        .collect::<String>();
    std::fs::write(&path, format!("{round}\n{errors}{first}\n{second}\n")).unwrap();
    let summary = run_journal(
        root.path(),
        &["digest", sid, "--format", "json", "--focus", "summary"],
    );
    assert!(summary.status.success(), "{summary:?}");
    let value: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(value["conflicting_terminal_count"], 1);
    let causes = value["other_errors"].as_array().unwrap();
    for expected in ["first terminal", "second terminal"] {
        assert!(
            causes
                .iter()
                .any(|cause| cause["detail"]["message"] == expected)
        );
    }
    assert!(causes.iter().any(|cause| {
        cause["kind"] == "observed_tool_error" && cause["detail"]["message"] == "permission denied"
    }));
    assert!(causes.iter().any(|cause| {
        cause["kind"] == "tool_error_observations_omitted" && cause["detail"]["omitted"] == 127
    }));
    assert!(!String::from_utf8_lossy(&summary.stdout).contains("SECRET_MARKER"));
    assert!(
        summary.stdout.len() < 16_000,
        "summary was {} bytes",
        summary.stdout.len()
    );
    let full = run_journal(
        root.path(),
        &["digest", sid, "--format", "json", "--focus", "all"],
    );
    assert!(full.status.success(), "{full:?}");
    assert!(String::from_utf8_lossy(&full.stdout).contains("SECRET_MARKER"));

    // Error-only conflicting terminals retain useful reasons under a fixed
    // output bound, even when many contain long Unicode/control text.
    let terminals = (0..8)
        .map(|n| {
            let message = match n {
                0 => "first terminal".to_owned(),
                1 => "second terminal".to_owned(),
                _ => format!("terminal-{n} \u{1}{}", "界".repeat(1_000)),
            };
            json!({"type":"turn_error", "ts":format!("t{n}"), "session_id":sid,
                "turn":1, "producer_scope":{"run_id":"run"}, "error":message})
        })
        .map(|event| format!("{event}\n"))
        .collect::<String>();
    std::fs::write(&path, terminals).unwrap();
    let summary = run_journal(
        root.path(),
        &["digest", sid, "--format", "json", "--focus", "summary"],
    );
    assert!(summary.status.success(), "{summary:?}");
    assert!(summary.stdout.len() < 16_000);
    let value: Value = serde_json::from_slice(&summary.stdout).unwrap();
    let causes = value["other_errors"].as_array().unwrap();
    for expected in ["first terminal", "second terminal"] {
        assert!(
            causes
                .iter()
                .any(|cause| cause["detail"]["message"] == expected)
        );
    }
    assert_eq!(
        causes
            .iter()
            .filter(|cause| cause["kind"] == "conflicting_terminal")
            .count(),
        4
    );
    assert!(causes.iter().any(|cause| {
        cause["kind"] == "conflicting_terminal_errors_omitted" && cause["detail"]["omitted"] == 4
    }));
    for cause in causes
        .iter()
        .filter(|cause| cause["kind"] == "conflicting_terminal")
    {
        assert!(cause["detail"]["message"].as_str().unwrap().chars().count() <= 160);
        assert!(cause["detail"].get("tool_calls").is_none());
    }

    // Conflicting terminals need not contain an error: the tool observation
    // may be the only retained reason, including when its top-level error is
    // absent but the call itself has one.
    let call = json!({"name":"bash","ok":false,"ms":1,"disposition":"executed",
        "tool_call_id":"call-1","error":"permission denied opening report",
        "args_preview":"SECRET_MARKER arguments",
        "invocation_identity":{"user_id":"owner","session_id":sid,"run_id":"run",
            "turn_chain_id":"chain","invocation_id":"call-1"}});
    let terminal = json!({"type":"turn","ts":"t2","session_id":sid,"turn":1,
        "producer_scope":{"run_id":"run"},"tool_count":1,"tokens_in":100,"tool_calls":[call]});
    let mut conflicting = terminal.clone();
    conflicting["ts"] = json!("t3");
    conflicting["tokens_in"] = json!(200);
    for source in ["event", "call", "result"] {
        let mut observed_call = call.clone();
        if source == "result" {
            observed_call["error"] = Value::Null;
            observed_call["result_preview"] = json!("permission denied opening report");
        }
        let mut error = json!({"type":"tool_call_error","ts":"t1","session_id":sid,"turn":1,
            "producer_scope":{"run_id":"run"},"tool_calls":[observed_call]});
        if source == "event" {
            error["error"] = json!("permission denied opening report");
        }
        std::fs::write(&path, format!("{error}\n{terminal}\n{conflicting}\n")).unwrap();
        let summary = run_journal(
            root.path(),
            &["digest", sid, "--format", "json", "--focus", "summary"],
        );
        assert!(summary.status.success(), "{summary:?}");
        let value: Value = serde_json::from_slice(&summary.stdout).unwrap();
        assert_eq!(value["conflicting_terminal_count"], 1);
        assert!(
            value["other_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|cause| {
                    cause["kind"] == "observed_tool_error"
                        && cause["ts"] == "t1"
                        && cause["detail"]["run_id_preview"] == "run"
                        && cause["detail"]["message"] == "permission denied opening report"
                })
        );
        assert!(!String::from_utf8_lossy(&summary.stdout).contains("SECRET_MARKER"));
    }

    // A separate ToolCallError may be missing from a partial journal. Both
    // round and terminal observations must retain a bounded reason, whether
    // or not the terminal itself has a conflicting projection.
    for has_conflict in [false, true] {
        for round_only in [false, true] {
            let mut observed_terminal = terminal.clone();
            let mut observed_conflict = conflicting.clone();
            let round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,
                "round":0,"producer_scope":{"run_id":"run"},"tool_calls":[call]});
            let mut lines = String::new();
            if round_only {
                observed_terminal["tool_calls"] = json!([]);
                observed_conflict["tool_calls"] = json!([]);
                lines.push_str(&format!("{round}\n"));
            }
            lines.push_str(&format!("{observed_terminal}\n"));
            if has_conflict {
                lines.push_str(&format!("{observed_conflict}\n"));
            }
            std::fs::write(&path, lines).unwrap();
            let summary = run_journal(
                root.path(),
                &["digest", sid, "--format", "json", "--focus", "summary"],
            );
            assert!(summary.status.success(), "{summary:?}");
            let value: Value = serde_json::from_slice(&summary.stdout).unwrap();
            assert_eq!(value["aggregates"]["observed_root_tool_failures"], 1);
            assert!(
                value["other_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|cause| {
                        cause["kind"] == "observed_tool_error"
                            && cause["detail"]["message"] == "permission denied opening report"
                    })
            );
            assert!(!String::from_utf8_lossy(&summary.stdout).contains("SECRET_MARKER"));
        }
    }

    let calls: Vec<_> = (0..6)
        .map(|n| {
            json!({"name":"bash", "ok":false, "ms":1,
            "disposition":"executed", "tool_call_id":format!("partial-{n}"),
            "error":format!("failure {n}")})
        })
        .collect();
    let round = json!({"type":"llm_round","ts":"t1","session_id":sid,"turn":1,
        "round":0,"producer_scope":{"run_id":"run"},"tool_calls":calls});
    let mut partial_terminal = terminal.clone();
    partial_terminal["tool_calls"] = json!([]);
    partial_terminal["tool_count"] = json!(6);
    std::fs::write(&path, format!("{round}\n{partial_terminal}\n")).unwrap();
    let summary = run_journal(
        root.path(),
        &["digest", sid, "--format", "json", "--focus", "summary"],
    );
    assert!(summary.status.success(), "{summary:?}");
    let value: Value = serde_json::from_slice(&summary.stdout).unwrap();
    let causes = value["other_errors"].as_array().unwrap();
    assert_eq!(
        causes
            .iter()
            .filter(|e| e["kind"] == "observed_tool_error")
            .count(),
        4
    );
    assert!(causes.iter().any(|e| {
        e["kind"] == "tool_error_observations_omitted" && e["detail"]["omitted"] == 2
    }));

    // Ordinary attempts use the same bound; otherwise a long journal still
    // turns a summary into an unbounded per-call report.
    let errors = (0..8)
        .map(|n| {
            json!({"type":"tool_call_error","ts":format!("t{n}"),"session_id":sid,"turn":1,
            "producer_scope":{"run_id":"run"},"error":format!("failure {n}")})
        })
        .map(|event| format!("{event}\n"))
        .collect::<String>();
    std::fs::write(&path, format!("{errors}{terminal}\n")).unwrap();
    let summary = run_journal(
        root.path(),
        &["digest", sid, "--format", "json", "--focus", "summary"],
    );
    assert!(summary.status.success(), "{summary:?}");
    let value: Value = serde_json::from_slice(&summary.stdout).unwrap();
    let causes = value["other_errors"].as_array().unwrap();
    assert_eq!(
        causes
            .iter()
            .filter(|e| e["kind"] == "observed_tool_error")
            .count(),
        4
    );
    // The terminal contributes a ninth distinct observed error.
    assert!(
        causes
            .iter()
            .any(|e| e["kind"] == "tool_error_observations_omitted" && e["detail"]["omitted"] == 5)
    );
    assert!(causes.iter().any(|e| e["detail"]["message"] == "failure 7"));
    assert!(!causes.iter().any(|e| e["detail"]["message"] == "failure 0"));

    // A repeated recent error must refresh its position and timestamp instead
    // of being evicted as though its only observation were the old one.
    let repeated = ["a", "b", "c", "d", "a", "e"]
        .into_iter()
        .enumerate()
        .map(|(n, error)| {
            json!({"type":"tool_call_error","ts":format!("t{n}"),"session_id":sid,"turn":1,
                "producer_scope":{"run_id":"run"},"error":error})
        })
        .map(|event| format!("{event}\n"))
        .collect::<String>();
    std::fs::write(&path, format!("{repeated}{terminal}\n")).unwrap();
    let summary = run_journal(
        root.path(),
        &["digest", sid, "--format", "json", "--focus", "summary"],
    );
    let value: Value = serde_json::from_slice(&summary.stdout).unwrap();
    let causes = value["other_errors"].as_array().unwrap();
    assert!(
        causes
            .iter()
            .any(|e| e["detail"]["message"] == "a" && e["ts"] == "t4")
    );
    assert!(!causes.iter().any(|e| e["detail"]["message"] == "b"));

    // Distinct reasons sharing a truncated prefix must not be treated as the
    // same observation. Missing reasons remain explicitly unavailable.
    let long = "界".repeat(200);
    let errors = [
        Some(format!("{long}a")),
        Some(format!("{long}b")),
        Some(format!("{long}a")),
        Some(format!(" {} ", "x".repeat(160))),
        None,
    ]
    .into_iter()
    .enumerate()
    .map(|(n, error)| {
        json!({"type":"tool_call_error","ts":format!("t{n}"),"session_id":sid,"turn":1,
            "metadata":{"run_id":"run"},"error":error})
    })
    .map(|event| format!("{event}\n"))
    .collect::<String>();
    std::fs::write(&path, format!("{errors}{terminal}\n")).unwrap();
    let summary = run_journal(
        root.path(),
        &["digest", sid, "--format", "json", "--focus", "summary"],
    );
    assert!(summary.status.success(), "{summary:?}");
    let value: Value = serde_json::from_slice(&summary.stdout).unwrap();
    let causes = value["other_errors"].as_array().unwrap();
    let observed: Vec<_> = causes
        .iter()
        .filter(|e| e["kind"] == "observed_tool_error")
        .collect();
    assert_eq!(observed.len(), 4);
    assert_eq!(
        observed
            .iter()
            .filter(|e| e["detail"]["message_truncated"] == true)
            .count(),
        2
    );
    assert!(observed.iter().any(|e| e["detail"]["message"].is_null()));
    assert!(observed.iter().any(|e| {
        e["detail"]["message"] == "x".repeat(160) && e["detail"]["message_truncated"] == false
    }));
    assert!(
        observed
            .iter()
            .all(|e| e["detail"]["run_id_preview"] == "run")
    );
    // One repeated error and the terminal's separate failure were omitted.
    assert!(
        causes
            .iter()
            .any(|e| e["kind"] == "tool_error_observations_omitted" && e["detail"]["omitted"] == 2)
    );
}
