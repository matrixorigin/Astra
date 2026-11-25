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
async fn find_expression_operands_use_the_shell_path_boundary() {
    let workspace = tempfile::tempdir().unwrap();
    let secret = workspace.path().join(".env");
    std::fs::write(&secret, "synthetic fixture").unwrap();
    let executor = executor(workspace.path());
    for command in [
        "find . -maxdepth 0 -fprint .env",
        "find . -maxdepth 0 -exec cat .env \\;",
    ] {
        let result = executor
            .execute_with_metadata("bash", &json!({"command": command}))
            .await;
        assert!(result.is_error, "{command}: {result:?}");
        assert_eq!(
            result.tool_result_fields.unwrap()["execution_started"],
            false
        );
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            "synthetic fixture"
        );
    }
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
        "execution_error"
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
async fn protected_path_preflight_reports_its_uncertainty_before_execution() {
    let fixture = tempfile::tempdir().unwrap();
    let executor = executor(fixture.path());
    let credential = fixture.path().join(".env");
    std::fs::write(&credential, "synthetic fixture").unwrap();
    std::fs::write(fixture.path().join("README.md"), "ordinary\n").unwrap();
    let app_config = fixture.path().join(".claude/settings.json");
    std::fs::create_dir(app_config.parent().unwrap()).unwrap();
    std::fs::write(&app_config, "[synthetic]\nkey = safe\n").unwrap();
    for policy in [Some(SandboxPolicy::permissive(fixture.path())), None] {
        *executor.sandbox_policy.write().unwrap() = policy;
        for (command, expected, rule) in [
            (
                "set -e\ntest ! -e \"$src/.git/objects/info/alternates\"\nprintf marker > marker",
                "could not establish",
                "write-sensitive app/runtime state",
            ),
            (
                "docker exec not-a-container cat .claude/settings.json; touch marker",
                "could not establish",
                "write-sensitive app/runtime state",
            ),
            (
                "touch -r .env marker",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "[[ x > .env ]]; touch marker",
                "could not establish",
                "sensitive credential or system path",
            ),
            (
                "test -f .env; touch marker",
                "could not establish",
                "sensitive credential or system path",
            ),
            (
                "grep needle .env; touch marker",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "uniq .env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "sort -o .claude/settings.json .env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "cut -c 1- .env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "paste .env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "wc --files0-from=.env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "file -f.env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "tree -H . --hintro=.env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "tree -o .env .",
                "blocked the write operand",
                "sensitive credential or system path",
            ),
            (
                "find -files0-from .env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "find . -name .env -delete",
                "could not establish",
                "sensitive credential or system path",
            ),
            (
                "find . -name .env -exec cat {} \\;",
                "could not establish",
                "sensitive credential or system path",
            ),
            (
                "find . -name .env -exec {} \\;",
                "could not establish",
                "sensitive credential or system path",
            ),
            (
                "find . -exec cat {} \\;",
                "could not establish",
                "sensitive credential or system path",
            ),
            (
                "find . -fprint \"$destination\"",
                "could not establish",
                "sensitive credential or system path",
            ),
            (
                "find README.md -files0-from paths -exec cat {} \\;",
                "could not establish",
                "sensitive credential or system path",
            ),
            (
                "git config --file=.env --list",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "git -C . config --fil=.env --list",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "git --git-dir=. config --fil=.env --list",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "git config --file=.claude/settings.json --add synthetic.key marker",
                "blocked the write operand",
                "write-sensitive app/runtime state",
            ),
            (
                "date --file=.env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "date --fi=.env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "date -uf.env",
                "blocked the read operand",
                "sensitive credential or system path",
            ),
            (
                "printf data > .git/config; touch marker",
                "blocked the write operand",
                "write-sensitive app/runtime state",
            ),
            (
                "printf data > .env; touch marker",
                "blocked the write operand",
                "sensitive credential or system path",
            ),
        ] {
            let result = executor
                .execute_with_metadata("bash", &json!({"command": command}))
                .await;
            assert!(result.is_error, "{command}: {result:?}");
            assert!(result.output.contains(expected), "{command}: {result:?}");
            assert!(result.output.contains(rule), "{command}: {result:?}");
            assert!(result.output.contains("command was not run"), "{result:?}");
            assert!(result.sandbox_approval_scope.is_none(), "{command}");
            let facts = result.tool_result_fields.unwrap();
            assert_eq!(facts["disposition"], "rejected", "{command}");
            assert_eq!(facts["execution_started"], false, "{command}");
            assert_eq!(facts["side_effects_maybe"], false, "{command}");
            assert_eq!(facts["error_kind"], "policy_denied", "{command}");
            assert_eq!(facts["recovery_evidence"]["retryable"], false, "{command}");
            assert!(!fixture.path().join("marker").exists());
            assert_eq!(
                std::fs::read_to_string(&credential).unwrap(),
                "synthetic fixture"
            );
            assert_eq!(
                std::fs::read_to_string(&app_config).unwrap(),
                "[synthetic]\nkey = safe\n"
            );
        }
        for command in ["uniq README.md", "cut -c 1- README.md", "paste README.md"] {
            let result = executor
                .execute_with_metadata("bash", &json!({"command":command}))
                .await;
            assert!(!result.is_error, "{command}: {result:?}");
            assert!(result.output.contains("ordinary"), "{command}: {result:?}");
        }
        let result = executor
            .execute_with_metadata(
                "bash",
                &json!({"command":"printf '%s\\n' '>' .env | head -n 2"}),
            )
            .await;
        assert!(
            !result.is_error && result.output.contains(".env"),
            "{result:?}"
        );
    }
    let config_read = executor
        .execute_with_metadata(
            "bash",
            &json!({"command":"git config --file=.claude/settings.json --list"}),
        )
        .await;
    assert!(!config_read.is_error, "{config_read:?}");
    assert!(config_read.output.contains("synthetic.key=safe"));
    assert_eq!(
        config_read.tool_result_fields.unwrap()["execution_started"],
        true
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tree_file_options_are_checked_without_blocking_literal_values() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join(".env"), "synthetic fixture").unwrap();
    std::fs::create_dir(fixture.path().join(".claude")).unwrap();
    let bin = fixture.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let tree = bin.join("tree");
    std::fs::write(&tree, "#!/bin/sh\nprintf 'tree fixture\\n'\n").unwrap();
    std::fs::set_permissions(&tree, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prefix = format!("PATH='{}':\"$PATH\"", bin.display());
    let executor = executor(fixture.path());

    for args in [
        "-H . --hintro=.env",
        "--houtro .env .",
        "--gitfile=.env .",
        "--infofile .env .",
        "--fromfile .env",
        "--fromtabfile .env",
        "-oF .env .",
        "-o .env .",
    ] {
        let command = format!("{prefix} tree {args}");
        let result = executor
            .execute_with_metadata("bash", &json!({"command": command}))
            .await;
        assert!(result.is_error, "{command}: {result:?}");
        assert_eq!(
            result.tool_result_fields.unwrap()["execution_started"],
            false,
            "{command}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path().join(".env")).unwrap(),
            "synthetic fixture"
        );
    }

    for args in ["-H .env .", "-T .env .", "-I .env .", ".claude"] {
        let command = format!("{prefix} tree {args}");
        let result = executor
            .execute_with_metadata("bash", &json!({"command": command}))
            .await;
        assert!(
            !result.is_error && result.output.contains("tree fixture"),
            "{command}: {result:?}"
        );
        assert_eq!(
            result.tool_result_fields.unwrap()["execution_started"],
            true,
            "{command}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_preflight_handles_gnu_tools_selected_by_path() {
    use std::{os::unix::fs::symlink, path::PathBuf};

    fn gnu_tool(names: &[&str]) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
            .find(|path| {
                Command::new(path)
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| {
                        output.status.success()
                            && String::from_utf8_lossy(&output.stdout).contains("GNU coreutils")
                    })
            })
    }

    let (Some(date), Some(paste), Some(wc)) = (
        gnu_tool(&["gdate", "date"]),
        gnu_tool(&["gpaste", "paste"]),
        gnu_tool(&["gwc", "wc"]),
    ) else {
        eprintln!("GNU date, paste or wc are unavailable; shared operand tests still run");
        return;
    };
    let sort = gnu_tool(&["gsort", "sort"]);
    let fixture = tempfile::tempdir().unwrap();
    let bin = fixture.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    symlink(date, bin.join("date")).unwrap();
    symlink(paste, bin.join("paste")).unwrap();
    symlink(wc, bin.join("wc")).unwrap();
    if let Some(sort) = &sort {
        symlink(sort, bin.join("sort")).unwrap();
    }
    std::fs::write(
        fixture.path().join(".env"),
        "SYNTHETIC_PRIVATE_MARKER=123\n",
    )
    .unwrap();
    std::fs::write(fixture.path().join("README.md"), "ordinary\n").unwrap();

    // Confirm the selected GNU binary would otherwise expose the fixture in stderr.
    let direct = Command::new(bin.join("date"))
        .arg("--file=.env")
        .current_dir(fixture.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&direct.stderr).contains("SYNTHETIC_PRIVATE_MARKER=123"));
    let direct_wc = Command::new(bin.join("wc"))
        .arg("--files0-from=.env")
        .current_dir(fixture.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&direct_wc.stderr).contains("SYNTHETIC_PRIVATE_MARKER=123"));

    let prefix = format!(
        "PATH='{}':\"$PATH\"",
        bin.display().to_string().replace('\'', "'\\''")
    );
    let executor = executor(fixture.path());
    for args in ["--file=.env", "--fi=.env", "-uf.env"] {
        let command = format!("{prefix} date {args}");
        let result = executor
            .execute_with_metadata("bash", &json!({"command": command}))
            .await;
        assert!(result.is_error, "{command}: {result:?}");
        assert!(!result.output.contains("SYNTHETIC_PRIVATE_MARKER"));
        assert_eq!(
            result.tool_result_fields.unwrap()["execution_started"],
            false,
            "{command}"
        );
    }
    for args in ["--files0-from=.env", "--files0-from .env", "--files=.env"] {
        let command = format!("{prefix} wc {args}");
        let result = executor
            .execute_with_metadata("bash", &json!({"command": command}))
            .await;
        assert!(
            result.is_error && !result.output.contains("SYNTHETIC_PRIVATE_MARKER"),
            "{command}: {result:?}"
        );
        assert_eq!(
            result.tool_result_fields.unwrap()["execution_started"],
            false,
            "{command}"
        );
    }
    for (command, expected) in [
        (format!("{prefix} date -d 2024-01-02 +%Y"), "2024"),
        (
            format!("{prefix} paste --delim .env README.md README.md"),
            "ordinary.ordinary",
        ),
    ] {
        let result = executor
            .execute_with_metadata("bash", &json!({"command": command}))
            .await;
        assert!(
            !result.is_error && result.output.contains(expected),
            "{command}: {result:?}"
        );
    }
    if sort.is_some() {
        let direct = Command::new(bin.join("sort"))
            .arg("--files0-from=.env")
            .current_dir(fixture.path())
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&direct.stderr).contains("SYNTHETIC_PRIVATE_MARKER=123"));
        for args in [
            "--files0-from=.env",
            "--files0-from .env",
            "--files=.env",
            "-o.env README.md",
            "-ro.env README.md",
            "--output=.env README.md",
            "--out .env README.md",
        ] {
            let command = format!("{prefix} sort {args}");
            let result = executor
                .execute_with_metadata("bash", &json!({"command": command}))
                .await;
            assert!(
                result.is_error && !result.output.contains("SYNTHETIC_PRIVATE_MARKER"),
                "{command}: {result:?}"
            );
            assert_eq!(
                result.tool_result_fields.unwrap()["execution_started"],
                false,
                "{command}"
            );
            assert_eq!(
                std::fs::read_to_string(fixture.path().join(".env")).unwrap(),
                "SYNTHETIC_PRIVATE_MARKER=123\n"
            );
        }
        let sorted = executor
            .execute_with_metadata(
                "bash",
                &json!({"command": format!("{prefix} sort README.md")}),
            )
            .await;
        assert!(
            !sorted.is_error && sorted.output.contains("ordinary"),
            "{sorted:?}"
        );
        let data_value = executor
            .execute_with_metadata(
                "bash",
                &json!({"command": format!("{prefix} sort --field-separator .env README.md")}),
            )
            .await;
        assert_eq!(
            data_value.tool_result_fields.unwrap()["execution_started"],
            true
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn bsd_date_format_values_remain_available() {
    let fixture = tempfile::tempdir().unwrap();
    let result = executor(fixture.path())
        .execute_with_metadata(
            "bash",
            &json!({
            "command": "/bin/date -j -f '%Y-%m-%d' 2024-01-02 +%Y"
                }),
        )
        .await;
    assert!(
        !result.is_error && result.output.contains("2024"),
        "{result:?}"
    );
}

#[tokio::test]
async fn protected_path_reads_work_in_supported_compounds_with_or_without_overlay() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    std::fs::write(root.join(".claude"), "fixture").unwrap();
    std::fs::write(root.join("README.md"), "mentions .env\n").unwrap();
    git(root, &["init", "-q"]);
    git(root, &["add", "-f", "--", ".claude"]);
    git(
        root,
        &[
            "-c",
            "user.name=Astra Test",
            "-c",
            "user.email=astra@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture .env commit",
        ],
    );
    let executor = executor(root);
    for policy in [Some(SandboxPolicy::permissive(root)), None] {
        *executor.sandbox_policy.write().unwrap() = policy;
        for (command, expected) in [
            ("git log --format=%s -- .claude", "fixture .env commit"),
            (
                "{ git log --format=%s -- .claude; } | head -n 1",
                "fixture .env commit",
            ),
            (
                "for rev in HEAD; do git log --format=%s \"$rev\" -- .claude; done",
                "fixture .env commit",
            ),
            (
                "git log --format=%s --grep .env -- .claude",
                "fixture .env commit",
            ),
            ("echo .env", ".env"),
            ("printf '%s\\n' .env", ".env"),
            ("grep -F .env README.md", "mentions .env"),
            ("touch -r .git/config marker; printf ok", "ok"),
        ] {
            let result = executor
                .execute_with_metadata("bash", &json!({"command": command}))
                .await;
            assert!(!result.is_error, "{command}: {result:?}");
            assert_eq!(result.output.trim(), expected, "{command}");
            assert_eq!(
                result.tool_result_fields.unwrap()["execution_started"],
                true
            );
        }
    }
}

#[tokio::test]
async fn credential_like_directory_names_do_not_block_ordinary_reads() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("service.envoy");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("service.envoy")).unwrap();
    std::fs::create_dir(root.join("output.env")).unwrap();
    std::fs::create_dir(root.join("cache.pem")).unwrap();
    std::fs::create_dir_all(root.join("fixtures/etc")).unwrap();
    std::fs::create_dir(root.join("service.aws")).unwrap();
    std::fs::create_dir(root.join(".aws")).unwrap();
    for path in [
        "fixtures/etc/passwd",
        "service.aws/credentials",
        ".aws/credentials",
    ] {
        std::fs::write(root.join(path), "fixture\n").unwrap();
    }
    std::fs::write(root.join("README.md"), "ordinary fixture\n").unwrap();
    std::fs::write(root.join(".env"), "private fixture\n").unwrap();
    let executor = executor(&root);
    for policy in [Some(SandboxPolicy::permissive(&root)), None] {
        *executor.sandbox_policy.write().unwrap() = policy;
        let ordinary = executor
            .execute_with_metadata("bash", &json!({"command":"cat README.md"}))
            .await;
        assert!(!ordinary.is_error, "{ordinary:?}");
        assert_eq!(ordinary.output, "ordinary fixture\n");
        for command in ["ls service.envoy", "ls output.env", "ls cache.pem"] {
            let result = executor
                .execute_with_metadata("bash", &json!({"command":command}))
                .await;
            assert!(!result.is_error, "{command}: {result:?}");
        }
        for (command, expected) in [
            ("test .env && printf yes", "yes"),
            ("basename .env", ".env\n"),
            ("dirname .env", ".\n"),
        ] {
            let result = executor
                .execute_with_metadata("bash", &json!({"command":command}))
                .await;
            assert!(!result.is_error, "{command}: {result:?}");
            assert_eq!(result.output, expected);
        }
        for command in ["cat fixtures/etc/passwd", "cat service.aws/credentials"] {
            let result = executor
                .execute_with_metadata("bash", &json!({"command":command}))
                .await;
            assert!(!result.is_error, "{command}: {result:?}");
            assert_eq!(result.output, "fixture\n");
        }
        let secret = executor
            .execute_with_metadata("bash", &json!({"command":"cat .env"}))
            .await;
        assert!(secret.is_error, "{secret:?}");
        let secret = executor
            .execute_with_metadata("bash", &json!({"command":"cat .aws/credentials"}))
            .await;
        assert!(secret.is_error, "{secret:?}");
    }
}

#[tokio::test]
async fn negative_ripgrep_globs_do_not_block_searches() {
    if Command::new("rg").arg("--version").output().is_err() {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/guide.txt"), "needle in public text\n").unwrap();
    std::fs::write(root.join("docs/example.env"), "needle in private fixture\n").unwrap();
    let executor = executor(root);
    let command = "rg -g '!*.env' needle docs";
    let result = executor
        .execute_with_metadata("bash", &json!({"command":command}))
        .await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.output.contains("public text"), "{result:?}");
    assert!(!result.output.contains("private fixture"), "{result:?}");
    let later_include = "rg -g '!*.env' -g '*.env' needle docs";
    let denied = executor
        .execute_with_metadata("bash", &json!({"command":later_include}))
        .await;
    assert!(denied.is_error, "{denied:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn protected_path_preflight_resolves_aliases_from_the_requested_workdir() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let nested = root.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(root.join(".env"), "synthetic secret").unwrap();
    std::fs::write(nested.join("ordinary"), "ordinary").unwrap();
    symlink("../.env", nested.join("alias")).unwrap();
    let executor = executor(root);

    for policy in [Some(SandboxPolicy::permissive(root)), None] {
        *executor.sandbox_policy.write().unwrap() = policy;
        let safe = executor
            .execute_with_metadata(
                "bash",
                &json!({"command": "cat ordinary", "workdir": "nested"}),
            )
            .await;
        assert!(!safe.is_error, "{safe:?}");
        assert_eq!(safe.output.trim(), "ordinary");
        let denied = executor
            .execute_with_metadata(
                "bash",
                &json!({"command": "cat alias", "workdir": "nested"}),
            )
            .await;
        assert!(denied.is_error, "{denied:?}");
        assert!(denied.output.contains("sensitive credential"), "{denied:?}");
        assert_eq!(
            denied.tool_result_fields.unwrap()["execution_started"],
            false
        );
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
