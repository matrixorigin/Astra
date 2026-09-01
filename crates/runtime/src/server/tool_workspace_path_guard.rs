use std::path::{Path, PathBuf};

use super::tool_execution_binding::{WorkspaceBinding, WorkspaceBindingKind};
use crate::tool_sandbox::{
    extract_local_workspace_path_mentions, is_shell_home_path, is_windows_drive_path,
};
use astra_sandbox::{canonicalize_parent_and_append, normalize_path};

pub(crate) fn unique_path_variants(path: &Path) -> Vec<PathBuf> {
    let mut variants = vec![normalize_path(path)];
    if let Ok(canonical) = canonicalize_parent_and_append(path)
        && !variants.iter().any(|existing| existing == &canonical)
    {
        variants.push(canonical);
    }
    variants
}

fn workspace_owns_absolute_path(workspace_root: &Path, raw_path: &str) -> bool {
    let candidate = Path::new(raw_path);
    if !candidate.is_absolute() {
        return false;
    }
    let candidate_variants = unique_path_variants(candidate);
    let workspace_variants = unique_path_variants(workspace_root);
    candidate_variants.iter().all(|candidate| {
        workspace_variants
            .iter()
            .any(|workspace| candidate == workspace || candidate.starts_with(workspace))
    })
}

pub(crate) fn server_sandbox_local_path_mismatch(
    command: &str,
    workspace_root: &Path,
    workspace_binding: &WorkspaceBinding,
) -> Option<String> {
    server_sandbox_local_path_mismatch_in_text(
        "command",
        command,
        workspace_root,
        workspace_binding,
    )
}

fn server_sandbox_local_path_mismatch_in_text(
    subject: &str,
    text: &str,
    workspace_root: &Path,
    workspace_binding: &WorkspaceBinding,
) -> Option<String> {
    if workspace_binding.kind != WorkspaceBindingKind::ServerSandbox {
        return None;
    }

    extract_local_workspace_path_mentions(text)
        .into_iter()
        .find(|path| {
            is_shell_home_path(path)
                || is_windows_drive_path(path)
                || !workspace_owns_absolute_path(workspace_root, path)
        })
        .map(|path| {
            workspace_path_mismatch_message(subject, &path, workspace_root, workspace_binding)
        })
}

fn workspace_path_mismatch_message(
    subject: &str,
    path: &str,
    workspace_root: &Path,
    workspace_binding: &WorkspaceBinding,
) -> String {
    let cwd = workspace_binding
        .cwd
        .as_deref()
        .unwrap_or_else(|| workspace_root.to_str().unwrap_or("current workspace"));
    format!(
        "Error: {subject} references local path '{path}', but the current workspace provider is rooted at {cwd}. Select a workspace provider rooted at that path, or use a path inside the current workspace, then retry."
    )
}

fn server_sandbox_path_argument_mismatch(
    subject: &str,
    raw_path: &str,
    workspace_root: &Path,
    workspace_binding: &WorkspaceBinding,
) -> Option<String> {
    if workspace_binding.kind != WorkspaceBindingKind::ServerSandbox {
        return None;
    }

    let path = raw_path.trim();
    if path.is_empty() {
        return None;
    }
    let candidate = Path::new(path);
    let mismatched = is_shell_home_path(path)
        || is_windows_drive_path(path)
        || (candidate.is_absolute() && !workspace_owns_absolute_path(workspace_root, path));
    if !mismatched {
        return None;
    }

    Some(workspace_path_mismatch_message(
        subject,
        path,
        workspace_root,
        workspace_binding,
    ))
}

fn path_arg<'a>(args: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    args.get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn server_sandbox_tool_path_mismatch(
    tool_name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
    workspace_binding: &WorkspaceBinding,
) -> Option<String> {
    if tool_name == "bash" {
        return path_arg(args, "command").and_then(|command| {
            server_sandbox_local_path_mismatch(command, workspace_root, workspace_binding)
        });
    }

    let fields: &[&str] = match tool_name {
        "read_file" | "write_file" | "str_replace" | "list_dir" | "symbols" => &["path"],
        "grep" => &["path"],
        "glob" => &["path", "pattern"],
        "git" => &["path", "file"],
        _ => &[],
    };

    fields.iter().find_map(|field| {
        let value = path_arg(args, field)?;
        let subject = format!("tool '{tool_name}' argument '{field}'");
        server_sandbox_path_argument_mismatch(&subject, value, workspace_root, workspace_binding)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn workspace_ownership_rejects_symlink_escape_even_when_lexical_path_is_inside() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let workspace_root = temp.path().join("workspace");
        let outside_root = temp.path().join("outside");
        std::fs::create_dir_all(&workspace_root).expect("workspace");
        std::fs::create_dir_all(&outside_root).expect("outside");
        symlink(&outside_root, workspace_root.join("linked-out")).expect("symlink");

        let escaped = workspace_root.join("linked-out/secret.txt");
        assert!(
            !workspace_owns_absolute_path(&workspace_root, &escaped.display().to_string()),
            "a path that canonicalizes outside the workspace must not be accepted just because its lexical spelling starts with the workspace root"
        );
    }
}
