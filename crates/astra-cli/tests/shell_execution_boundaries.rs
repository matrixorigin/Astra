//! Shell admission and execution facts are independent of optional observation.
use astra_cli::edge_tools::{ToolExecutor, all_tool_schemas};
use astra_runtime::tool_sandbox::SandboxPolicy;
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn executor(root: &Path) -> ToolExecutor {
    let executor = ToolExecutor::new(root);
    executor.set_current_visible_tool_schemas(&all_tool_schemas());
    *executor.sandbox_policy.write().unwrap() = Some(SandboxPolicy::permissive(root));
    executor
}

fn git(root: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(result.status.success(), "{args:?}: {result:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn repeated_failed_builds_preserve_diagnostics_without_inventing_progress() {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir().unwrap();
    let go = workspace.path().join("go");
    std::fs::write(
        &go,
        "#!/bin/sh\nprintf '%s\\n' 'pkg/compile.go:27:2: cannot find module providing package example/fault' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&go, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executor = executor(workspace.path());
    for _ in 0..2 {
        let result = executor
            .execute_with_metadata("bash", &json!({"command": "PATH=\"$PWD:$PATH\" go build"}))
            .await;
        assert_eq!(result.tool_result_fields.as_ref().unwrap()["exit_code"], 1);
        assert!(result.output.contains("cannot find module"), "{result:?}");
        assert!(!result.output.contains("0 error(s)"), "{result:?}");
        assert!(!result.output.contains("build succeeded"), "{result:?}");
        assert!(!result.output.contains("Iteration"), "{result:?}");
    }
    for (name, script, code, diagnostic) in [
        (
            "cargo",
            "printf 'Finished first target\\nerror[E0425]: missing name\\n' >&2; exit 101",
            101,
            "error[E0425]: missing name",
        ),
        (
            "pytest",
            "printf '1 failed, 2 passed\\n'; exit 1",
            1,
            "1 failed, 2 passed",
        ),
    ] {
        let path = workspace.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let command = format!(r#"PATH="$PWD:$PATH" {name}"#);
        let result = executor
            .execute_with_metadata("bash", &json!({"command":command}))
            .await;
        assert_eq!(
            result.tool_result_fields.as_ref().unwrap()["exit_code"],
            code
        );
        assert!(
            result.output.contains(diagnostic)
                && result.output.contains(&format!("exit code {code}")),
            "{result:?}"
        );
        assert!(
            !result.output.contains("build succeeded") && !result.output.contains("0 failed"),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn standalone_shell_waits_for_terminal_and_rejects_unusable_stdin() {
    let workspace = tempfile::tempdir().unwrap();
    let executor = std::sync::Arc::new(executor(workspace.path()));
    let running = executor.clone();
    let mut call = tokio::spawn(async move {
        running
            .execute_with_metadata(
                "bash",
                &json!({
                    "command":"touch started; while [ ! -f release ]; do sleep 0.01; done; printf finished",
                    "timeout":5
                }),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !workspace.path().join("started").exists() {
            tokio::select! {
                result = &mut call => panic!("command ended before starting: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    })
    .await
    .expect("command started");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut call)
            .await
            .is_err(),
        "a standalone executor cannot return a live task it will abandon"
    );
    std::fs::write(workspace.path().join("release"), b"").unwrap();
    let result = call.await.unwrap();
    assert!(!result.is_error, "{result:?}");
    assert!(result.output.contains("finished"));
    assert_eq!(result.tool_result_fields.unwrap()["exit_code"], 0);

    for args in [
        json!({"command":"touch unintended", "stdin":true}),
        json!({"command":"touch unintended", "yield_time_ms":0}),
    ] {
        let rejected = executor.execute_with_metadata("bash", &args).await;
        assert!(rejected.is_error);
        let fields = rejected.tool_result_fields.unwrap();
        assert_eq!(fields["execution_started"], false);
        assert_eq!(fields["side_effects_maybe"], false);
        assert!(!workspace.path().join("unintended").exists());
    }
}

#[tokio::test]
async fn standalone_cli_reports_truncation_and_exit_without_workspace_artifacts() {
    let workspace = tempfile::tempdir().unwrap();
    let executor = executor(workspace.path());
    let output = executor
        .execute_with_metadata("bash", &json!({"command":"seq 1 40000; printf '\\377password=example-secret\\nerror: TAIL_SENTINEL\\nOpen http://127.0.0.1:3000'; seq 1 40000 >&2; printf 'error: STDERR_SENTINEL\\n' >&2; sleep 30 & exit 7"}))
        .await;
    assert!(output.is_error, "{output:?}");
    let fields = output.tool_result_fields.as_ref().unwrap();
    assert_eq!(fields["execution_started"], true);
    assert_eq!(fields["exit_code"], 7);
    assert_eq!(fields["background_children_reaped"], true);
    assert!(
        output.output.contains("additional output is unavailable"),
        "{output:?}"
    );
    assert!(output.output.contains("TAIL_SENTINEL"), "{output:?}");
    assert!(output.output.contains("STDERR_SENTINEL"), "{output:?}");
    assert!(output.output.contains("127.0.0.1:3000"), "{output:?}");
    assert!(output.output.contains("non-UTF-8"), "{output:?}");
    assert!(!output.output.contains("example-secret"), "{output:?}");
    assert!(!workspace.path().join("astra-bash-outputs").exists());
    let next = executor
        .execute_with_metadata("bash", &json!({"command":"printf settled"}))
        .await;
    assert!(!next.is_error, "{next:?}");
    assert_eq!(next.output.trim(), "settled");
}

#[tokio::test]
async fn standalone_cli_cancel_keeps_partial_output_and_terminal_fact() {
    let workspace = tempfile::tempdir().unwrap();
    let executor = executor(workspace.path());
    let token = tokio_util::sync::CancellationToken::new();
    let args = json!({"command":"printf 'before-cancel\\n'; touch started; sleep 30"});
    let call = executor.execute_with_metadata_cancelable("bash", &args, Some(&token));
    tokio::pin!(call);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut call => panic!("command must still be running: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    if workspace.path().join("started").exists() { break; }
                }
            }
        }
    })
    .await
    .expect("command must reach its started marker");
    token.cancel();
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .unwrap();
    assert!(output.is_error, "{output:?}");
    let fields = output.tool_result_fields.as_ref().unwrap();
    assert_eq!(fields["error_kind"], "cancelled");
    assert_eq!(fields["execution_started"], true);
    assert!(output.output.contains("output is incomplete"), "{output:?}");
    assert!(output.output.contains("before-cancel"), "{output:?}");
    assert!(!workspace.path().join("astra-bash-outputs").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn signal_does_not_expose_an_incomplete_credential() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("signal_helper.py"),
        "import os, signal\nos.write(1, b'SIGNAL_FIXTURE_STARTED\\npassword=abc')\nos.kill(os.getpid(), signal.SIGKILL)\n",
    )
    .unwrap();
    let executor = executor(workspace.path());
    for prefix in ["", "seq 1 20000; "] {
        let command = format!("{prefix}python3 signal_helper.py");
        let result = executor
            .execute_with_metadata("bash", &json!({"command":command}))
            .await;
        assert!(result.is_error, "{result:?}");
        assert_eq!(
            result.tool_result_fields.as_ref().unwrap()["execution_started"],
            true
        );
        assert_eq!(
            result.tool_result_fields.as_ref().unwrap()["exit_code"],
            137
        );
        assert!(
            result.output.contains("SIGNAL_FIXTURE_STARTED"),
            "{result:?}"
        );
        assert!(!result.output.contains("password=abc"), "{result:?}");
    }
    let truncated = executor
        .execute_with_metadata(
            "bash",
            &json!({"command":"printf 'PIPELINE_STARTED\\n'; set -o pipefail; yes password=abc | head -c 12"}),
        )
        .await;
    assert_eq!(
        truncated.tool_result_fields.as_ref().unwrap()["execution_started"],
        true
    );
    assert_eq!(
        truncated.tool_result_fields.as_ref().unwrap()["exit_semantics"],
        "pipeline_truncated"
    );
    assert!(
        truncated.output.contains("PIPELINE_STARTED"),
        "{truncated:?}"
    );
    assert!(!truncated.output.contains("password=abc"), "{truncated:?}");
}

#[tokio::test]
async fn standalone_cli_discloses_non_utf8_without_claiming_byte_exact_output() {
    let workspace = tempfile::tempdir().unwrap();
    let result = executor(workspace.path())
        .execute_with_metadata("bash", &json!({"command":"printf '\\377'"}))
        .await;
    assert!(!result.is_error, "{result:?}");
    assert!(
        result.output.contains("additional output is unavailable"),
        "{result:?}"
    );
    assert!(!workspace.path().join("astra-bash-outputs").exists());
}

#[tokio::test]
async fn authorized_worktree_cleanup_and_absence_probe_do_not_require_a_directory_snapshot() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path().join("repo");
    let checkout = fixture.path().join("review");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet"]);
    std::fs::write(repo.join(".gitignore"), "cache/\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            checkout.to_str().unwrap(),
        ],
    );
    std::fs::create_dir(checkout.join("cache")).unwrap();
    // Sparse fixture exceeds both existing content-scan budgets without a large allocation.
    std::fs::File::create(checkout.join("cache/artifact.bin"))
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let executor = executor(&repo);
    let cleanup = executor
        .execute_with_metadata(
            "bash",
            &json!({
                "command": format!("git worktree remove --force '{}'", checkout.display()),
            }),
        )
        .await;
    assert!(!cleanup.is_error, "{cleanup:?}");
    assert!(!checkout.exists());
    let facts = cleanup.tool_result_fields.unwrap();
    assert_eq!(facts["execution_started"], true);
    assert_eq!(facts["disposition"], "executed");
    assert_eq!(facts["exit_code"], 0);
    git(&repo, &["worktree", "list", "--porcelain"]);

    // Already absent is a valid desired state. Observe it in a separate call;
    // do not manufacture another mutation to establish a before/after delta.
    let observed = executor
        .execute_with_metadata(
            "bash",
            &json!({
                "command": format!("test ! -e '{}' && printf absent", checkout.display()),
            }),
        )
        .await;
    assert!(!observed.is_error, "{observed:?}");
    assert_eq!(observed.output.trim(), "absent");
}

#[tokio::test]
async fn source_preservation_and_argument_refusals_never_start_the_command() {
    let fixture = tempfile::tempdir().unwrap();
    let executor = executor(fixture.path());
    executor.set_active_session_id(uuid::Uuid::new_v4().to_string());
    for args in [
        json!({"command": "touch marker", "source_artifacts": ["missing-input"]}),
        json!({"command": " "}),
    ] {
        let rejected = executor.execute_with_metadata("bash", &args).await;
        assert!(rejected.is_error, "{args:?}: {rejected:?}");
        assert!(!fixture.path().join("marker").exists());
        let facts = rejected.tool_result_fields.unwrap();
        assert_eq!(facts["disposition"], "rejected");
        assert_eq!(facts["execution_started"], false);
        assert_eq!(facts["side_effects_maybe"], false);
    }
}

#[tokio::test]
async fn nonzero_exit_preserves_partial_effects_and_executed_failure() {
    let fixture = tempfile::tempdir().unwrap();
    let executor = executor(fixture.path());
    let failed = executor
        .execute_with_metadata(
            "bash",
            &json!({
                "command": "printf partial > marker; exit 7",
            }),
        )
        .await;
    assert!(failed.is_error, "{failed:?}");
    assert_eq!(
        std::fs::read_to_string(fixture.path().join("marker")).unwrap(),
        "partial"
    );
    let facts = failed.tool_result_fields.unwrap();
    assert_eq!(facts["disposition"], "executed");
    assert_eq!(facts["execution_started"], true);
    assert_eq!(facts["exit_code"], 7);
    assert_eq!(facts["side_effects_maybe"], true);
}

#[tokio::test]
async fn cancellation_before_dispatch_is_not_an_executed_failure() {
    let fixture = tempfile::tempdir().unwrap();
    let executor = executor(fixture.path());
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let cancelled = executor
        .execute_with_metadata_cancelable("bash", &json!({"command": "touch marker"}), Some(&token))
        .await;
    assert!(cancelled.is_error);
    assert!(!fixture.path().join("marker").exists());
    let fields = cancelled.tool_result_fields.unwrap();
    assert_eq!(fields["disposition"], "rejected");
    assert_eq!(fields["execution_started"], false);
    assert_eq!(fields["side_effects_maybe"], false);
    assert_eq!(fields["error_kind"], "cancelled");
}

#[tokio::test]
async fn foreground_timeout_preserves_external_effect_and_typed_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let marker = external.path().join("marker");
    let executor = executor(workspace.path());
    let failed = executor
        .execute_with_metadata(
            "bash",
            &json!({
                "command": format!("printf partial >> '{}'; sleep 1; printf late >> '{}'; sleep 5", marker.display(), marker.display()),
                "timeout": 0.2,
            }),
        )
        .await;
    assert!(failed.is_error, "{failed:?}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "partial");
    let fields = failed.tool_result_fields.unwrap();
    assert_eq!(fields["error_kind"], "tool_timeout");
    assert_eq!(fields["execution_started"], true);
    assert_eq!(fields["disposition"], "executed");
    assert_eq!(fields["side_effects_maybe"], true);
    assert_eq!(fields["retryable"], false);
    assert_eq!(fields["recovery_evidence"]["kind"], "tool_timeout");
    assert_eq!(fields["recovery_evidence"]["retryable"], false);
    assert_eq!(
        fields["recovery_evidence"]["recovery_actions"],
        json!(["inspect_structured_failure"])
    );
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "partial",
        "timed-out process must not resume its effects"
    );
}
