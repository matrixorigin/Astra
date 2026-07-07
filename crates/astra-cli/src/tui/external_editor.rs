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
    edit_in_external_editor_with_command_in(initial, command_override, None)
}

fn edit_in_external_editor_with_command_in(
    initial: &str,
    command_override: Option<&str>,
    temp_dir: Option<&std::path::Path>,
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
    let file = match temp_dir {
        Some(dir) => tempfile::NamedTempFile::new_in(dir),
        None => tempfile::NamedTempFile::new(),
    }
    .map_err(|e| format!("create temp draft: {e}"))?;
    std::fs::write(file.path(), initial).map_err(|e| format!("write temp draft: {e}"))?;
    let status = build_editor_process(&editor, file.path())?
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

fn build_editor_process(editor: &str, target: &std::path::Path) -> Result<Command, String> {
    if requires_shell_evaluation(editor) {
        let mut command = Command::new("sh");
        command
            .arg("-lc")
            .arg(r#"editor_cmd=$1; target=$2; eval "$editor_cmd \"\$target\"""#)
            .arg("astra-editor")
            .arg(editor)
            .arg(target)
            .env("ASTRA_EDITOR_TARGET", target);
        return Ok(command);
    }

    let tokens =
        shell_words::split(editor).map_err(|e| format!("parse external editor command: {e}"))?;
    if tokens.is_empty() {
        return Err("external editor command is empty".to_string());
    }

    let mut command_index = 0usize;
    while command_index < tokens.len() && looks_like_env_assignment(&tokens[command_index]) {
        command_index += 1;
    }
    if command_index == tokens.len() {
        return Err("external editor command must include a program".to_string());
    }

    let mut command = Command::new(&tokens[command_index]);
    command.args(&tokens[command_index + 1..]);
    for assignment in &tokens[..command_index] {
        let (key, value) = assignment
            .split_once('=')
            .expect("env assignment already validated");
        command.env(key, value);
    }
    command.env("ASTRA_EDITOR_TARGET", target).arg(target);
    Ok(command)
}

fn looks_like_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn requires_shell_evaluation(command: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;

    for ch in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => {
                escaped = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '$' | '`' if !in_single => {
                return true;
            }
            '&' | '|' | ';' | '<' | '>' | '(' | ')' if !in_single && !in_double => {
                return true;
            }
            _ => {}
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{
        edit_in_external_editor_with_command_in, first_available_editor, requires_shell_evaluation,
    };

    #[test]
    fn external_editor_roundtrips_updated_text() {
        let dir = crate::tests::test_temp_dir();
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

        let edited = edit_in_external_editor_with_command_in(
            "before",
            Some(&format!("sh {}", script.display())),
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(edited, "edited from editor\n");
    }

    #[test]
    fn external_editor_supports_quoted_paths_and_env_prefixes() {
        let temp = crate::tests::test_temp_dir();
        let dir = temp.path().join("external editor path");
        std::fs::create_dir(&dir).unwrap();
        let script = dir.join("editor script.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n[ \"$EDITOR_MODE\" = \"test\" ] || exit 9\nprintf 'edited with args\\n' > \"$1\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let edited = edit_in_external_editor_with_command_in(
            "before",
            Some(&format!("EDITOR_MODE=test sh '{}'", script.display())),
            Some(temp.path()),
        )
        .unwrap();
        assert_eq!(edited, "edited with args\n");
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

    #[test]
    fn simple_editor_commands_do_not_require_shell() {
        assert!(!requires_shell_evaluation("sh /tmp/editor.sh"));
        assert!(!requires_shell_evaluation(
            "EDITOR_MODE=test sh '/tmp/editor script.sh'"
        ));
    }

    #[test]
    fn compound_editor_commands_require_shell() {
        assert!(requires_shell_evaluation(
            "EDITOR_MODE=test sh editor.sh && echo done"
        ));
        assert!(requires_shell_evaluation("sh -lc \"nvim \\\"$1\\\"\""));
    }
}
