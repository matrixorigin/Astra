use std::path::PathBuf;

use super::ToolExecutor;

// ── expand_sandbox_path ──────────────────────────────────────────────────

#[test]
fn expand_sandbox_path_adds_and_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Before expansion: /etc is not allowed
    assert!(
        !exe.sandbox_policy
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_path_allowed(std::path::Path::new("/etc/passwd"))
    );
    assert!(exe.resolve_checked("/etc/passwd").is_err());

    // Expand
    exe.expand_sandbox_path(PathBuf::from("/etc"));

    // After expansion: /etc is allowed via both APIs
    assert!(
        exe.sandbox_policy
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_path_allowed(std::path::Path::new("/etc/passwd"))
    );
    assert!(exe.resolve_checked("/etc/passwd").is_ok());
}

#[test]
fn expand_sandbox_path_noop_without_policy() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    *exe.sandbox_policy.write().unwrap() = None;
    // Should not panic
    exe.expand_sandbox_path(PathBuf::from("/etc"));
}

#[test]
fn expand_sandbox_to_root_opens_everything() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    // Expanding to "/" opens the entire filesystem — this is why
    // stream_render.rs must never pass "/" to expand_sandbox_path.
    exe.expand_sandbox_path(PathBuf::from("/"));
    assert!(exe.resolve_checked("/etc/passwd").is_ok());
    assert!(exe.resolve_checked("/var/secret").is_ok());
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

    exe.expand_sandbox_path(external);

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
