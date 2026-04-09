use super::*;

// ── expand_sandbox_path ──────────────────────────────────────────────────

#[test]
fn expand_sandbox_path_adds_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut exe = ToolExecutor::new(dir.path());
    // Before expansion: /etc is not allowed
    assert!(
        !exe.sandbox_policy
            .as_ref()
            .unwrap()
            .is_path_allowed(std::path::Path::new("/etc/passwd"))
    );
    // Expand
    exe.expand_sandbox_path(PathBuf::from("/etc"));
    // After expansion: /etc is allowed
    assert!(
        exe.sandbox_policy
            .as_ref()
            .unwrap()
            .is_path_allowed(std::path::Path::new("/etc/passwd"))
    );
}

#[test]
fn expand_sandbox_path_noop_without_policy() {
    let dir = tempfile::tempdir().unwrap();
    let mut exe = ToolExecutor::new(dir.path());
    exe.sandbox_policy = None;
    // Should not panic
    exe.expand_sandbox_path(PathBuf::from("/etc"));
}

#[test]
fn expand_sandbox_then_resolve_checked_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let mut exe = ToolExecutor::new(dir.path());
    // Before: /etc/passwd is blocked
    assert!(exe.resolve_checked("/etc/passwd").is_err());
    // Expand to /etc
    exe.expand_sandbox_path(PathBuf::from("/etc"));
    // After: /etc/passwd is allowed
    assert!(exe.resolve_checked("/etc/passwd").is_ok());
}

#[test]
fn expand_sandbox_to_root_opens_everything() {
    let dir = tempfile::tempdir().unwrap();
    let mut exe = ToolExecutor::new(dir.path());
    // Expanding to "/" opens the entire filesystem — this is why
    // stream_render.rs must never pass "/" to expand_sandbox_path.
    exe.expand_sandbox_path(PathBuf::from("/"));
    assert!(exe.resolve_checked("/etc/passwd").is_ok());
    assert!(exe.resolve_checked("/var/secret").is_ok());
}
