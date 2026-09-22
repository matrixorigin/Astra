use std::process::Command;

#[cfg(unix)]
#[path = "../src/test_support.rs"]
mod test_support;

#[cfg(unix)]
#[test]
fn preflight_and_cases_share_binary_and_inherited_proxy_policy() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let work = root.join("other-checkout");
    let credentials = root.join("credentials");
    let suite = root.join("cases");
    for path in [root.join("bin"), work.join("bin"), suite.clone()] {
        std::fs::create_dir_all(path).unwrap();
    }
    let _guard = astra_credentials::set_test_credentials_dir(credentials.clone());
    astra_credentials::CredentialStore::new()
        .mutate(|store| {
            store.profiles.insert(
                "fixture".into(),
                astra_credentials::Profile {
                    account_id: Some("fixture-account".into()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    let session = "550e8400-e29b-41d4-a716-446655440000";
    let case_session = "550e8400-e29b-41d4-a716-446655440001";
    std::fs::write(
        suite.join("case.yaml"),
        "name: routing\nprompt: case-prompt\n",
    )
    .unwrap();
    // The real entrypoint archives the case journal before cleanup. Publish
    // fixture evidence through the canonical owner-scoped path resolver.
    let _journal_guard =
        astra_services::session_journal::JournalDirGuard::new(root.join("state/sessions"));
    let journal = astra_services::session_journal::journal_file_path_for_owner(
        &astra_services::OwnerScope::user("fixture-account").unwrap(),
        case_session,
    )
    .unwrap();
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(&journal, format!(
        "{{\"type\":\"session_start\",\"ts\":\"2026-09-22T00:00:00Z\",\"session_id\":\"{case_session}\"}}\n{{\"type\":\"session_end\",\"ts\":\"2026-09-22T00:00:01Z\",\"session_id\":\"{case_session}\"}}\n"
    )).unwrap();
    let outcome = serde_json::json!({
        "trace_id": null, "request_id": null, "run_id": "run-1", "session_id": session,
        "text": "pong", "final_state": "completed", "interruption_kind": null,
        "tool_result_class_counts": {}, "prompt_tokens": 0, "fresh_prompt_tokens": 0,
        "cache": {"hit": false, "read_tokens": 0, "creation_tokens": 0},
        "completion_tokens": 0, "llm_rounds": 0, "tool_calls_count": 0, "tools_used": [],
        "persistence_error": null, "exit_code": 0, "success": true, "error_kind": null
    });
    let mut case_outcome = outcome.clone();
    case_outcome["session_id"] = case_session.into();
    // A successful fixture requires unchanged bypass variables on every CLI
    // invocation, including the authentication retry and session cleanup.
    test_support::write_executable_shim(
        &root.join("bin/astra"),
        format!(
            r#"#!/bin/sh
[ "${{NO_PROXY-unset}}" = "$EXPECTED_UPPER" ] || exit 91
[ "${{no_proxy-unset}}" = "$EXPECTED_LOWER" ] || exit 92
printf '%s\n' "$*" >> "$CALLS"
[ "$1" = --profile ] && shift 2
case "$1" in
health) printf '%s' '{{"status":"healthy","database":"connected","interaction_api_major":"3","build_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","build_git_dirty":false}}' ;;
chat)
  if [ ! -f "$REGISTERED" ]; then printf '401 Unauthorized' >&2; exit 3; fi
  events=; next_is_events=0
  for arg in "$@"; do
    if [ "$next_is_events" = 1 ]; then events=$arg; next_is_events=0;
    elif [ "$arg" = --stream-events ]; then next_is_events=1; fi
  done
  if [ -n "$events" ]; then
    printf '%s\n' '{{"type":"session_bound","session_id":"{case_session}"}}' > "$events"
    printf '%s' '{case_outcome}'
  else
    printf '%s' '{outcome}'
  fi
  ;;
admin) if [ "$2" = login ]; then touch "$REGISTERED"; fi ;;
session) printf '{{"session_id":"%s","status":"cancelled","execution_settled":true}}' "$3" ;;
*) exit 99 ;;
esac
"#
        ),
    )
    .unwrap();
    test_support::write_executable_shim(
        &work.join("bin/astra"),
        "#!/bin/sh\nprintf wrong-binary >> \"$CALLS\"\nexit 93\n",
    )
    .unwrap();

    for source in ["flag", "env", "path"] {
        for bypass in [true, false] {
            let calls = root.join(format!("calls-{source}-{bypass}"));
            let mut command = Command::new(env!("CARGO_BIN_EXE_astra-test"));
            command
                .env_clear()
                .env("PATH", "bin:/usr/bin:/bin")
                .env("ASTRA_CONFIG_SOURCE", "explicit-env")
                .env("ASTRA_CLI_CREDENTIALS_DIR", &credentials)
                .env("ASTRA_LOCAL_STATE_ROOT", root.join("state"))
                .env("ASTRA_API_URL", "http://astra.internal")
                .env("HTTP_PROXY", "http://proxy.invalid:3128")
                .env("CALLS", &calls)
                .env(
                    "REGISTERED",
                    root.join(format!("registered-{source}-{bypass}")),
                )
                .env(
                    "EXPECTED_UPPER",
                    if bypass { "astra.internal" } else { "unset" },
                )
                .env(
                    "EXPECTED_LOWER",
                    if bypass { "other.internal" } else { "unset" },
                )
                .current_dir(root)
                .arg("--suite")
                .arg(&suite)
                .arg("--working-dir")
                .arg(&work)
                .args([
                    "--profile",
                    "fixture",
                    "--models",
                    "fixture-model",
                    "--no-judger",
                    "--no-digest-on-fail",
                ]);
            if bypass {
                command
                    .env("NO_PROXY", "astra.internal")
                    .env("no_proxy", "other.internal");
            }
            match source {
                "flag" => {
                    command.args(["--astra-bin", "./bin/astra"]);
                }
                "env" => {
                    command.env("ASTRA_BIN", "./bin/astra");
                }
                _ => {}
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{source}/{bypass}: {output:?}");
            let log = std::fs::read_to_string(calls).unwrap();
            for expected in [
                "health",
                "admin register",
                "admin login",
                "chat -m ping",
                "chat -m case-prompt",
                "session cancel",
                "session delete",
            ] {
                assert!(log.contains(expected), "missing {expected}: {log}");
            }
            assert!(!log.contains("wrong-binary"), "{log}");
            assert_eq!(log.matches("chat -m ping").count(), 2, "{log}");
            assert!(
                log.contains(&format!("session delete {case_session}")),
                "{log}"
            );
        }
    }
}

#[test]
fn executable_identity_precedes_configuration_and_argument_parsing() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_astra-test"))
        .arg("--build-info-json")
        .current_dir(directory.path())
        .env(
            "ASTRA_CONFIG_SOURCE",
            "invalid-configuration-must-not-be-read",
        )
        .env("ASTRA_EXPECTED_BUILD_GIT_SHA", "invalid")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::to_value(astra_core::build_info::current()).unwrap(),
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn requested_revision_cannot_be_certified_with_skipped_preflight() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_astra-test"))
        .arg("--skip-preflight")
        .current_dir(directory.path())
        .env(
            "ASTRA_EXPECTED_BUILD_GIT_SHA",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot bypass"),
        "{output:?}"
    );
}

#[test]
fn requested_revision_rejects_execution_paths_without_identity_verification() {
    let directory = tempfile::tempdir().unwrap();
    for arguments in [["--live-dashboard", "17899"], ["--executor-cmd", "false"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_astra-test"))
            .args(arguments)
            .current_dir(directory.path())
            .env(
                "ASTRA_EXPECTED_BUILD_GIT_SHA",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("requires the built-in CLI executor"),
            "{output:?}"
        );
    }
}

#[test]
fn case_routing_overrides_are_rejected_before_preflight_or_model_execution() {
    let directory = tempfile::tempdir().unwrap();
    let suite = directory.path().join("cases");
    std::fs::create_dir(&suite).unwrap();
    // Deliberately not executable: revision-bound case validation must fail
    // before even attempting the artifact/health/model probes of this CLI.
    let cli = directory.path().join("astra");
    std::fs::write(&cli, "must never be executed").unwrap();
    for configuration in [
        "cli_env: {ASTRA_API_URL: 'http://case-target.invalid'}",
        "cli_env: {ASTRA_PROFILE: other}",
        "cli_env: {https_proxy: 'http://case-proxy.invalid'}",
        "extra_cli_args: ['--api-url=http://case-target.invalid']",
        "extra_cli_args: ['--profile', other]",
    ] {
        std::fs::write(
            suite.join("routing.yaml"),
            format!(
                "name: routing\nprompt: hello\n{configuration}\nsteps:\n  - prompt: continue\n"
            ),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_astra-test"))
            .arg("--suite")
            .arg(&suite)
            .arg("--astra-bin")
            .arg(&cli)
            .args([
                "--profile",
                "local",
                "--models",
                "test-model",
                "--no-judger",
            ])
            .current_dir(directory.path())
            .env("ASTRA_API_URL", "http://preflight-target.invalid")
            .env(
                "ASTRA_EXPECTED_BUILD_GIT_SHA",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("can change the verified execution target"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("case-target.invalid"),
            "must not disclose values: {stderr}"
        );
        assert!(
            !stderr.contains("binary exists but is not executable"),
            "{stderr}"
        );
    }
}
