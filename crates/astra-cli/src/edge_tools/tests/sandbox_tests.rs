use std::path::PathBuf;

use super::ToolExecutor;

// ── expand_sandbox_path ──────────────────────────────────────────────────

#[test]
fn expand_sandbox_path_adds_and_resolves() {
    // Use tempdir_in(CWD) so the external dir is NOT under std::env::temp_dir()
    // (which is pre-allowed by default_temp_allowed_paths) — this isolates the
    // test to the expansion behavior rather than the default temp allowance.
    let base = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let project = base.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let exe = ToolExecutor::new(&project);

    let external_path = base.path().join("external");
    std::fs::create_dir(&external_path).unwrap();
    let target = external_path.join("notes.md");
    std::fs::write(&target, "old\n").unwrap();
    let target_str = target.to_string_lossy().into_owned();

    // Before expansion: external file is not allowed.
    assert!(
        !exe.sandbox_policy
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_path_allowed(&target)
    );
    assert!(exe.resolve_checked(&target_str).is_err());

    // Expand
    exe.expand_sandbox_path(external_path.clone()).unwrap();

    // After expansion: target is allowed via both APIs.
    assert!(
        exe.sandbox_policy
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_path_allowed(&target)
    );
    assert!(exe.resolve_checked(&target_str).is_ok());
}

#[test]
fn expand_sandbox_path_fails_closed_without_policy() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    *exe.sandbox_policy.write().unwrap() = None;
    let external = tempfile::tempdir().unwrap();
    // Fail-closed: when no policy is installed, the expansion MUST error
    // rather than silently returning Ok. The old behavior returned Ok(dir)
    // without mutating any sandbox — the user believed `--add-dir` took
    // effect, but the sandbox was never updated.
    let err = exe
        .expand_sandbox_path(external.path().to_path_buf())
        .expect_err("missing sandbox policy must fail-closed, not silently no-op");
    assert!(matches!(
        err,
        crate::edge_tools::SandboxExpansionError::NoSandboxPolicy
    ));
}

#[test]
fn expand_sandbox_path_fails_closed_on_poisoned_lock() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    // Poison the sandbox_policy lock: acquire write guard and panic inside
    // catch_unwind. The unwind drops the guard, marking the RwLock poisoned.
    // The next write() must surface as PolicyLockPoisoned rather than
    // silently dropping the request.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = exe.sandbox_policy.write().unwrap();
        panic!("intentional poison");
    }));

    let external = tempfile::tempdir().unwrap();
    let err = exe
        .expand_sandbox_path(external.path().to_path_buf())
        .expect_err("poisoned sandbox_policy lock must fail-closed");
    assert!(
        matches!(
            err,
            crate::edge_tools::SandboxExpansionError::PolicyLockPoisoned
        ),
        "expected PolicyLockPoisoned, got {err:?}"
    );
}

#[test]
fn expand_sandbox_path_rejects_root() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    // Expanding to "/" would open the entire filesystem — reject.
    let err = exe
        .expand_sandbox_path(PathBuf::from("/"))
        .expect_err("root must be rejected");
    assert!(matches!(
        err,
        crate::edge_tools::SandboxExpansionError::RootPath
    ));
    // Filesystem must remain closed.
    assert!(exe.resolve_checked("/etc/passwd").is_err());
    assert!(exe.resolve_checked("/var/secret").is_err());
}

#[test]
fn expand_sandbox_path_rejects_system_sensitive_paths() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    for sys in ["/etc", "/etc/passwd", "/etc/ssh", "/var/run/secrets"] {
        let err = exe
            .expand_sandbox_path(PathBuf::from(sys))
            .expect_err("system-sensitive path must be rejected");
        assert!(
            matches!(
                err,
                crate::edge_tools::SandboxExpansionError::SystemSensitivePath
            ),
            "expected SystemSensitivePath for {sys}, got {err:?}"
        );
    }
}

#[test]
fn expand_sandbox_path_rejects_parent_dir_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    // Absolute path with a `..` component — must be rejected as a traversal
    // escape, not as NotAbsolute.
    let traversal = dir.path().join("subdir/..");
    let err = exe
        .expand_sandbox_path(traversal)
        .expect_err("parent-dir traversal must be rejected");
    assert!(matches!(
        err,
        crate::edge_tools::SandboxExpansionError::TraversalEscape
    ));
}

#[test]
fn expand_sandbox_path_rejects_relative_path() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    // Only absolute, concrete paths are expandable.
    let err = exe
        .expand_sandbox_path(PathBuf::from("relative/path"))
        .expect_err("relative path must be rejected");
    assert!(matches!(
        err,
        crate::edge_tools::SandboxExpansionError::NotAbsolute
    ));
}

#[test]
fn write_file_existing_external_file_respects_expanded_sandbox_boundary() {
    let base = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let project = base.path().join("project");
    let external = base.path().join("external");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&external).unwrap();
    let target = external.join("notes.md");
    std::fs::write(&target, "old\n").unwrap();

    let exe = ToolExecutor::new(&project);
    let before_expand = exe.read_file(&serde_json::json!({
        "path": target.to_string_lossy()
    }));
    assert!(
        crate::sandbox_retry::is_sandbox_denied(&before_expand),
        "{before_expand}"
    );

    exe.expand_sandbox_path(external).unwrap();

    let read = exe.read_file(&serde_json::json!({
        "path": target.to_string_lossy()
    }));
    assert!(read.contains("old"), "{read}");

    let result = exe.write_file(&serde_json::json!({
        "path": target.to_string_lossy(),
        "content": "new\n"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("write_file json");

    assert_eq!(parsed["success"].as_bool(), Some(true), "{result}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
}

#[tokio::test]
async fn execute_with_metadata_sandbox_denial_is_structured_and_hides_wire_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Use a non-sensitive path outside the sandbox boundary so the denial
    // routes through the structured sandbox-denial path (not the
    // never-readable short-circuit which /etc/passwd now triggers).
    let outcome = exe
        .execute_with_metadata(
            "read_file",
            &serde_json::json!({"path": "/var/astra-sandbox-test-nonexistent"}),
        )
        .await;

    assert!(outcome.is_error);
    assert!(
        !outcome
            .output
            .contains(crate::sandbox_retry::SANDBOX_DENIED_PREFIX),
        "{}",
        outcome.output
    );
    assert!(outcome.output.starts_with("Error: "), "{}", outcome.output);
    let fields = outcome.tool_result_fields.expect("metadata fields");
    assert_eq!(
        fields.get("error_kind").and_then(serde_json::Value::as_str),
        Some(crate::sandbox_retry::SANDBOX_DENIED_ERROR_KIND)
    );
}

#[tokio::test]
async fn cancelable_bash_sandbox_denial_is_structured_and_hides_wire_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let outcome = exe
        .execute_with_metadata_cancelable(
            "bash",
            &serde_json::json!({"command": "cat /var/astra-sandbox-test-nonexistent"}),
            None,
        )
        .await;

    assert!(outcome.is_error);
    assert!(
        !outcome
            .output
            .contains(crate::sandbox_retry::SANDBOX_DENIED_PREFIX),
        "{}",
        outcome.output
    );
    let fields = outcome.tool_result_fields.expect("metadata fields");
    assert_eq!(
        fields.get("error_kind").and_then(serde_json::Value::as_str),
        Some(crate::sandbox_retry::SANDBOX_DENIED_ERROR_KIND)
    );
}
