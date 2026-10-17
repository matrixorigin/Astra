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
        json!(["reconcile_outcome"])
    );
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "partial",
        "timed-out process must not resume its effects"
    );
}
