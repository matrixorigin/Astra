use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CliContext {
    /// Caller intent, distinct from the resolved/default model used for UI and budgets.
    pub(crate) requested_model_policy: Option<astra_turn_types::RequestedModelPolicy>,
    pub(crate) no_journal_content: bool,
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
        allowed_tools: &[String],
        disallowed_tools: &[String],
        add_dirs: &[String],
        auto_approve: bool,
        session_id: Option<String>,
        session_name: Option<String>,
    ) -> Result<Self, String> {
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
            requested_model_policy: None,
            no_journal_content,
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

    /// Called only by an explicit model flag or picker/clear action, never by a resolver.
    pub(crate) fn select_model(&mut self, model: Option<&str>) {
        self.requested_model_policy = Some(
            astra_core::model_override::normalize_model_override(model)
                .map(|model| astra_turn_types::RequestedModelPolicy::Fixed {
                    selector: astra_turn_types::ModelSelector::ConfiguredName {
                        model_name:
                            astra_turn_core::thinking_config::resolve_model_thinking_request(model)
                                .0
                                .to_string(),
                        source: None,
                    },
                })
                .unwrap_or(astra_turn_types::RequestedModelPolicy::Inherit),
        );
    }
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
    fn explicit_model_intent_is_separate_from_defaults_and_can_be_cleared() {
        use astra_turn_types::{ModelSelector, RequestedModelPolicy};
        let mut context = CliContext::default();
        assert_eq!(context.requested_model_policy, None);
        context.select_model(Some("model-a(thinking:high)"));
        assert_eq!(
            context.requested_model_policy,
            Some(RequestedModelPolicy::Fixed {
                selector: ModelSelector::ConfiguredName {
                    model_name: "model-a".into(),
                    source: None
                },
            })
        );
        context.select_model(None);
        assert_eq!(
            context.requested_model_policy,
            Some(RequestedModelPolicy::Inherit)
        );
    }

    #[test]
    fn from_launch_options_normalizes_tool_lists() {
        let ctx = temp_env::with_vars(
            [
                ("ASTRA_CLI_SESSION_ID", None::<&str>),
                ("ASTRA_CLI_SESSION_NAME", None::<&str>),
                ("ASTRA_CLI_ALLOWED_TOOLS", None::<&str>),
                ("ASTRA_CLI_DISALLOWED_TOOLS", None::<&str>),
            ],
            || {
                CliContext::from_launch_options(
                    false,
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

        assert_eq!(ctx.allowed_tools, vec!["bash", "view", "rg"]);
        assert_eq!(ctx.disallowed_tools, vec!["read_file", "edit_file"]);
    }

    #[test]
    fn from_launch_options_rejects_invalid_session_id() {
        temp_env::with_vars([("ASTRA_CLI_SESSION_ID", None::<&str>)], || {
            let err = CliContext::from_launch_options(
                false,
                &[],
                &[],
                &[],
                false,
                Some("not-a-uuid".into()),
                None,
            )
            .expect_err("invalid session id should fail");

            assert!(err.contains("ASTRA_CLI_SESSION_ID/--session-id must be a valid UUID"));
        });
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
                    let ctx =
                        CliContext::from_launch_options(false, &[], &[], &[], false, None, None)
                            .expect("cli context");

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
            let err = CliContext::from_launch_options(false, &[], &[], &[], false, None, None)
                .expect_err("invalid env session id should fail");
            assert!(err.contains("ASTRA_CLI_SESSION_ID/--session-id must be a valid UUID"));
        });
    }
}
