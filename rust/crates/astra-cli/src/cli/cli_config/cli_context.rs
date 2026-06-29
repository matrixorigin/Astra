use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CliContext {
    pub(crate) no_journal_content: bool,
    pub(crate) max_turns: Option<u32>,
    pub(crate) allowed_tools: Vec<String>,
    pub(crate) disallowed_tools: Vec<String>,
    pub(crate) add_dirs: Vec<PathBuf>,
    pub(crate) auto_approve: bool,
    pub(crate) permission_mode: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) session_name: Option<String>,
}

impl CliContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_launch_options(
        no_journal_content: bool,
        max_turns: Option<usize>,
        allowed_tools: &[String],
        disallowed_tools: &[String],
        add_dirs: &[String],
        auto_approve: bool,
        session_id: Option<String>,
        session_name: Option<String>,
    ) -> Result<Self, String> {
        let max_turns = resolve_max_turns(max_turns)?;
        let session_id = resolve_optional_env_value(session_id, "ASTRA_CLI_SESSION_ID");
        let session_name = resolve_optional_env_value(session_name, "ASTRA_CLI_SESSION_NAME");

        if let Some(ref sid) = session_id
            && uuid::Uuid::parse_str(sid).is_err()
        {
            return Err(format!(
                "Error: ASTRA_CLI_SESSION_ID/--session-id must be a valid UUID, got '{sid}'"
            ));
        }

        Ok(Self {
            no_journal_content,
            max_turns,
            allowed_tools: resolve_tool_list(allowed_tools, "ASTRA_CLI_ALLOWED_TOOLS"),
            disallowed_tools: resolve_tool_list(disallowed_tools, "ASTRA_CLI_DISALLOWED_TOOLS"),
            add_dirs: resolve_add_dirs(add_dirs),
            auto_approve,
            permission_mode: None,
            session_id,
            session_name,
        })
    }

    pub(crate) fn with_permission_mode(mut self, permission_mode: Option<String>) -> Self {
        self.permission_mode = permission_mode;
        self
    }
}

fn resolve_max_turns(max_turns: Option<usize>) -> Result<Option<u32>, String> {
    let raw = match max_turns {
        Some(value) => Some(value.to_string()),
        None => std::env::var("ASTRA_CLI_MAX_TURNS")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    };
    let Some(raw) = raw else {
        return Ok(None);
    };
    let parsed = raw.parse::<u32>().map_err(|_| {
        format!(
            "Error: ASTRA_CLI_MAX_TURNS/--max-turns must be a non-negative integer, got '{raw}'"
        )
    })?;
    Ok(Some(parsed))
}

fn resolve_optional_env_value(value: Option<String>, env_key: &str) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var(env_key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn resolve_tool_list(values: &[String], env_key: &str) -> Vec<String> {
    if !values.is_empty() {
        return normalize_tool_list(values);
    }
    std::env::var(env_key)
        .ok()
        .map(|value| normalize_tool_list(&[value]))
        .unwrap_or_default()
}

fn resolve_add_dirs(values: &[String]) -> Vec<PathBuf> {
    if !values.is_empty() {
        return canonicalize_dirs(values);
    }
    let env_values: Vec<String> = std::env::var_os("ASTRA_CLI_ADD_DIRS")
        .map(|value| {
            std::env::split_paths(&value)
                .map(|path| path.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    canonicalize_dirs(&env_values)
}

fn normalize_tool_list(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split([',', ' ']))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn canonicalize_dirs(values: &[String]) -> Vec<PathBuf> {
    values
        .iter()
        .map(|value| {
            Path::new(value).canonicalize().unwrap_or_else(|error| {
                tracing::warn!(
                    path = %value,
                    error = %error,
                    "failed to canonicalize add-dir path; keeping original value"
                );
                PathBuf::from(value)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{CliContext, canonicalize_dirs};
    use std::path::PathBuf;

    #[test]
    fn from_launch_options_normalizes_tool_lists() {
        let ctx = temp_env::with_vars(
            [
                ("ASTRA_CLI_MAX_TURNS", None::<&str>),
                ("ASTRA_CLI_SESSION_ID", None::<&str>),
                ("ASTRA_CLI_SESSION_NAME", None::<&str>),
                ("ASTRA_CLI_ALLOWED_TOOLS", None::<&str>),
                ("ASTRA_CLI_DISALLOWED_TOOLS", None::<&str>),
            ],
            || {
                CliContext::from_launch_options(
                    false,
                    Some(12),
                    &["bash, view".into(), "rg".into()],
                    &["read_file edit_file".into()],
                    &[],
                    false,
                    None,
                    None,
                )
                .expect("cli context")
            },
        );

        assert_eq!(ctx.max_turns, Some(12));
        assert_eq!(ctx.allowed_tools, vec!["bash", "view", "rg"]);
        assert_eq!(ctx.disallowed_tools, vec!["read_file", "edit_file"]);
    }

    #[test]
    fn from_launch_options_rejects_invalid_session_id() {
        temp_env::with_vars(
            [
                ("ASTRA_CLI_MAX_TURNS", None::<&str>),
                ("ASTRA_CLI_SESSION_ID", None::<&str>),
            ],
            || {
                let err = CliContext::from_launch_options(
                    false,
                    None,
                    &[],
                    &[],
                    &[],
                    false,
                    Some("not-a-uuid".into()),
                    None,
                )
                .expect_err("invalid session id should fail");

                assert!(err.contains("ASTRA_CLI_SESSION_ID/--session-id must be a valid UUID"));
            },
        );
    }

    #[test]
    fn canonicalize_dirs_keeps_original_when_missing() {
        let dirs = canonicalize_dirs(&["./definitely-missing-dir".into()]);
        assert_eq!(dirs, vec![PathBuf::from("./definitely-missing-dir")]);
    }

    #[test]
    fn from_launch_options_uses_env_fallbacks() {
        let add_dir = tempfile::TempDir::new().expect("tempdir");
        let joined_paths = std::env::join_paths([add_dir.path()]).expect("join paths");
        temp_env::with_vars(
            [
                ("ASTRA_CLI_MAX_TURNS", Some("27")),
                ("ASTRA_CLI_ALLOWED_TOOLS", Some("bash, view rg")),
                ("ASTRA_CLI_DISALLOWED_TOOLS", Some("write_file edit_file")),
                (
                    "ASTRA_CLI_SESSION_ID",
                    Some("123e4567-e89b-12d3-a456-426614174000"),
                ),
                ("ASTRA_CLI_SESSION_NAME", Some("env-session")),
            ],
            || {
                temp_env::with_var("ASTRA_CLI_ADD_DIRS", Some(joined_paths.clone()), || {
                    let ctx = CliContext::from_launch_options(
                        false,
                        None,
                        &[],
                        &[],
                        &[],
                        false,
                        None,
                        None,
                    )
                    .expect("cli context");

                    assert_eq!(ctx.max_turns, Some(27));
                    assert_eq!(ctx.allowed_tools, vec!["bash", "view", "rg"]);
                    assert_eq!(ctx.disallowed_tools, vec!["write_file", "edit_file"]);
                    assert_eq!(
                        ctx.add_dirs,
                        vec![
                            add_dir
                                .path()
                                .canonicalize()
                                .expect("canonicalize temp dir path")
                        ]
                    );
                    assert_eq!(
                        ctx.session_id.as_deref(),
                        Some("123e4567-e89b-12d3-a456-426614174000")
                    );
                    assert_eq!(ctx.session_name.as_deref(), Some("env-session"));
                });
            },
        );
    }

    #[test]
    fn from_launch_options_prefers_flags_over_env() {
        temp_env::with_vars(
            [
                ("ASTRA_CLI_ALLOWED_TOOLS", Some("bash,view")),
                ("ASTRA_CLI_SESSION_NAME", Some("env-session")),
            ],
            || {
                let ctx = CliContext::from_launch_options(
                    false,
                    None,
                    &["rg".into()],
                    &[],
                    &[],
                    false,
                    None,
                    Some("flag-session".into()),
                )
                .expect("cli context");

                assert_eq!(ctx.allowed_tools, vec!["rg"]);
                assert_eq!(ctx.session_name.as_deref(), Some("flag-session"));
            },
        );
    }

    #[test]
    fn from_launch_options_rejects_invalid_env_session_id() {
        temp_env::with_var("ASTRA_CLI_SESSION_ID", Some("not-a-uuid"), || {
            let err =
                CliContext::from_launch_options(false, None, &[], &[], &[], false, None, None)
                    .expect_err("invalid env session id should fail");
            assert!(err.contains("ASTRA_CLI_SESSION_ID/--session-id must be a valid UUID"));
        });
    }

    #[test]
    fn from_launch_options_rejects_invalid_env_max_turns() {
        temp_env::with_var("ASTRA_CLI_MAX_TURNS", Some("abc"), || {
            let err =
                CliContext::from_launch_options(false, None, &[], &[], &[], false, None, None)
                    .expect_err("invalid env max turns should fail");
            assert!(err.contains("ASTRA_CLI_MAX_TURNS/--max-turns"));
        });
    }
}
