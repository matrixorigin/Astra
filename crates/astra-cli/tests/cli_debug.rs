use astra_credentials::{CredentialStore, Profile, local_profile_owner_id};
use astra_services::{
    OwnerScope,
    session_journal::{JournalDirGuard, JournalEvent, JournalWriter},
};
use serde_json::json;
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_reads_attached_journals_and_pairs_checkpoints_in_append_order() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/auth/me"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(json!({"user_id":"debug-account"})),
        )
        .mount(&server)
        .await;
    let api_url = server.uri();
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("state");
    let _journals = JournalDirGuard::new(state_root.join("sessions"));
    let account = OwnerScope::user("debug-account").unwrap();
    let profile =
        OwnerScope::user(local_profile_owner_id("default", Some(account.id())).unwrap()).unwrap();
    let credentials = root.path().join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    CredentialStore::with_path(credentials.join("credentials.json"))
        .mutate(|file| {
            file.current_profile = Some("default".into());
            file.profiles.insert(
                "default".into(),
                Profile {
                    account_id: Some(account.id().into()),
                    access_token: Some("offline-test-token".into()),
                    ..Profile::default()
                },
            );
        })
        .unwrap();
    let write_checkpoint =
        |owner: &OwnerScope, session: &str, number: u32, history: &[serde_json::Value]| {
            let mut recorder =
                astra_pipeline::step_recorder::StepRecorder::new(owner.id(), session, "debug-task");
            recorder.begin_turn(number);
            let heavy = recorder
                .build_heavy_checkpoint(history, 100, 10, &[], &[])
                .unwrap();
            astra_pipeline::step_checkpoint::write_step_checkpoint(
                owner.id(),
                session,
                number,
                &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
            )
            .unwrap();
        };
    let write_source = |owner: &OwnerScope, session: &str, messages: &[(&str, &str)]| {
        let writer = JournalWriter::for_owner(owner, session).unwrap();
        let mut history = Vec::new();
        for (index, (text, timestamp)) in messages.iter().enumerate() {
            let mut event = JournalEvent::turn(
                Some(session),
                index as u32 + 1,
                None,
                text,
                "done",
                0,
                12,
                3,
                2,
            );
            event.ts = (*timestamp).into();
            writer.append(&event).unwrap();
            history.push(json!({"role":"user", "content":text}));
            write_checkpoint(owner, session, index as u32 + 1, &history);
        }
    };
    let run = |session: &str, input: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_astra"));
        for (key, _) in
            std::env::vars().filter(|(key, _)| key.starts_with("ASTRA_") || key.starts_with("MOI_"))
        {
            command.env_remove(key);
        }
        let mut child = command
            .args(["--api-url", api_url.as_str(), "debug", session])
            .current_dir(root.path())
            .env("HOME", root.path())
            .env("ASTRA_LOCAL_STATE_ROOT", &state_root)
            .env("ASTRA_CLI_CREDENTIALS_DIR", &credentials)
            .env("MOI_AUTH_DIR", root.path().join("auth"))
            .env("ASTRA_CONFIG_SOURCE", "explicit-env")
            .env("ASTRA_API_URL", &api_url)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    let timestamp = "2026-01-01T00:00:00Z";
    for (owner, text) in [
        (&profile, "profile-only-evidence"),
        (&account, "account-only-evidence"),
    ] {
        let session = uuid::Uuid::new_v4().to_string();
        write_source(owner, &session, &[(text, timestamp)]);
        let output = run(&session, "1\n1\nb\nq\n");
        assert!(output.contains(text), "{output}");
        assert!(!output.contains("No data found"), "{output}");
    }
    let session = uuid::Uuid::new_v4().to_string();
    write_source(
        &profile,
        &session,
        &[("profile-paired-evidence", timestamp)],
    );
    write_source(
        &account,
        &session,
        &[("account-paired-evidence", timestamp)],
    );
    let output = run(&session, "1\n6\n1\n5\n7\nb\nn\n1\n6\n1\n5\n7\nb\nq\n");
    assert!(
        output.contains("profile-paired-evidence") && output.contains("account-paired-evidence"),
        "{output}"
    );
    let profile_summary = output.find("user:     profile-paired-evidence").unwrap();
    let account_source = output.rfind("Source:").unwrap();
    assert!(profile_summary < account_source, "{output}");
    assert!(
        output[profile_summary..account_source]
            .matches("profile-paired-evidence")
            .count()
            >= 2,
        "{output}"
    );
    assert!(
        !output[profile_summary..account_source].contains("account-paired-evidence"),
        "{output}"
    );
    assert!(
        output[account_source..]
            .matches("account-paired-evidence")
            .count()
            >= 2,
        "{output}"
    );
    assert!(
        !output[account_source..].contains("profile-paired-evidence"),
        "{output}"
    );
    let exports = output
        .lines()
        .filter_map(|line| {
            line.split_once("Written to ")
                .map(|(_, path)| path.split('\u{1b}').next().unwrap().trim())
        })
        .collect::<Vec<_>>();
    assert_eq!(exports.len(), 4, "{output}");
    for (index, path) in exports.iter().enumerate() {
        let payload: serde_json::Value = serde_json::from_slice(
            &std::fs::read(path)
                .unwrap_or_else(|error| panic!("export {path:?}: {error}; output: {output}")),
        )
        .unwrap();
        let (owner, text) = if index < 2 {
            (&profile, "profile-paired-evidence")
        } else {
            (&account, "account-paired-evidence")
        };
        assert_eq!(payload["owner_id"], owner.id());
        assert_eq!(payload["session_id"], session);
        let messages = if index % 2 == 0 {
            &payload["messages_delta"]
        } else {
            &payload["messages"]
        };
        assert_eq!(messages, &json!([{"role":"user", "content":text}]));
        std::fs::remove_file(path).unwrap();
    }
    // No journal exists: exercise the checkpoint-only display and export entrypoint.
    let checkpoint_session = uuid::Uuid::new_v4().to_string();
    write_checkpoint(
        &profile,
        &checkpoint_session,
        1,
        &[json!({"role":"user","content":"checkpoint-only-evidence"})],
    );
    let output = run(&checkpoint_session, "1\n7\nb\n");
    assert!(
        output.contains("No journal turns") && output.contains("checkpoint-only-evidence"),
        "{output}"
    );
    let path = output
        .lines()
        .find_map(|line| {
            line.split_once("Written to ")
                .map(|(_, path)| path.split('\u{1b}').next().unwrap().trim())
        })
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(
        &std::fs::read(path)
            .unwrap_or_else(|error| panic!("export {path:?}: {error}; output: {output}")),
    )
    .unwrap();
    assert_eq!(payload["owner_id"], profile.id());
    assert_eq!(payload["session_id"], checkpoint_session);
    assert_eq!(
        payload["messages"],
        json!([{"role":"user","content":"checkpoint-only-evidence"}])
    );
    std::fs::remove_file(path).unwrap();
    let clock_session = uuid::Uuid::new_v4().to_string();
    write_source(
        &profile,
        &clock_session,
        &[
            ("first-appended-evidence", "2030-01-01T00:00:00Z"),
            ("second-appended-evidence", "2020-01-01T00:00:00Z"),
        ],
    );
    let output = run(&clock_session, "1\n6\n1\nb\n2\n6\n1\nb\nq\n");
    assert!(
        output.contains("first-appended-evidence") && output.contains("second-appended-evidence"),
        "{output}"
    );
    let first_summary = output.find("user:     first-appended-evidence").unwrap();
    let second_summary = output.find("user:     second-appended-evidence").unwrap();
    assert!(first_summary < second_summary, "{output}");
    assert!(
        output[first_summary..second_summary]
            .matches("first-appended-evidence")
            .count()
            >= 2,
        "{output}"
    );
    assert!(
        !output[first_summary..second_summary].contains("second-appended-evidence"),
        "{output}"
    );
    assert!(
        output[second_summary..]
            .matches("second-appended-evidence")
            .count()
            >= 2,
        "{output}"
    );
    let private_session = uuid::Uuid::new_v4().to_string();
    write_source(
        &OwnerScope::user("unrelated-account").unwrap(),
        &private_session,
        &[("private-evidence", timestamp)],
    );
    let output = run(&private_session, "q\n");
    assert!(
        output.contains("No data found") && !output.contains("private-evidence"),
        "{output}"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 6);
    assert!(
        requests
            .iter()
            .all(|request| request.method == "GET" && request.url.path() == "/auth/me")
    );
}
