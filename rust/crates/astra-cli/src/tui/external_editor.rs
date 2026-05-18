use std::process::Command;

pub(crate) fn edit_in_external_editor(initial: &str) -> Result<String, String> {
    edit_in_external_editor_with_command(initial, resolve_editor_command().as_deref())
}

fn resolve_editor_command() -> Option<String> {
    configured_editor_command()
        .or_else(git_core_editor_command)
        .or_else(fallback_editor_command)
}

fn configured_editor_command() -> Option<String> {
    std::env::var("VISUAL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

fn git_core_editor_command() -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", "core.editor"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let editor = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if editor.is_empty() {
        None
    } else {
        Some(editor)
    }
}

fn fallback_editor_command() -> Option<String> {
    first_available_editor(&["nvim", "vim", "vi", "nano"], binary_exists_in_path)
}

fn first_available_editor<F>(candidates: &[&str], mut exists: F) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    candidates
        .iter()
        .copied()
        .find(|candidate| exists(candidate))
        .map(str::to_string)
}

fn binary_exists_in_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(binary).is_file())
}

fn edit_in_external_editor_with_command(
    initial: &str,
    command_override: Option<&str>,
) -> Result<String, String> {
    let editor = command_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(resolve_editor_command)
        .ok_or_else(|| {
            "no external editor found; set $VISUAL/$EDITOR or install one of: nvim, vim, vi, nano"
                .to_string()
        })?;
    let file = tempfile::NamedTempFile::new().map_err(|e| format!("create temp draft: {e}"))?;
    std::fs::write(file.path(), initial).map_err(|e| format!("write temp draft: {e}"))?;
    let status = Command::new("sh")
        .arg("-lc")
        .arg(format!(r#"{editor} "$ASTRA_EDITOR_TARGET""#))
        .env("ASTRA_EDITOR_TARGET", file.path())
        .status()
        .map_err(|e| format!("launch external editor: {e}"))?;
    if !status.success() {
        return Err(format!(
            "external editor exited with status {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        ));
    }
    std::fs::read_to_string(file.path()).map_err(|e| format!("read edited draft: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_editor_roundtrips_updated_text() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("editor.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'edited from editor\\n' > \"$ASTRA_EDITOR_TARGET\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let edited = edit_in_external_editor_with_command(
            "before",
            Some(&format!("sh {}", script.display())),
        )
        .unwrap();
        assert_eq!(edited, "edited from editor\n");
    }

    #[test]
    fn first_available_editor_picks_first_present_candidate() {
        let picked = first_available_editor(&["nvim", "vim", "nano"], |binary| binary == "vim");
        assert_eq!(picked.as_deref(), Some("vim"));
    }

    #[test]
    fn first_available_editor_returns_none_when_no_candidate_exists() {
        let picked = first_available_editor(&["nvim", "vim"], |_| false);
        assert!(picked.is_none());
    }
}
