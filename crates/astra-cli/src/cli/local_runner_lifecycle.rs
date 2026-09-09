use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

#[cfg(unix)]
use astra_edge::local_host::{Attachment, Installation, ManagedClient};

/// Owns this CLI's attachment, not the shared host process. Dropping a view
/// releases only its environment slot; the host drains durable inference.
pub(crate) struct ManagedLocalRunner {
    #[cfg(unix)]
    client: ManagedClient,
}

impl ManagedLocalRunner {
    pub(crate) async fn wait_until_alive(&mut self, _timeout: Duration) -> Result<(), String> {
        #[cfg(unix)]
        if self.client.is_alive() {
            return Ok(());
        }
        Err("Local model connection ended. Reopen Astra to reconnect; retained inference remains recoverable.".into())
    }

    pub(crate) fn edge_id(&self) -> &str {
        #[cfg(unix)]
        {
            &self.client.attachment.runner_id
        }
        #[cfg(not(unix))]
        {
            ""
        }
    }

    #[cfg(unix)]
    pub(crate) fn attachment(&self) -> &Attachment {
        &self.client.attachment
    }

    pub(crate) async fn stop(self) {
        drop(self);
    }

    pub(crate) fn attach_context(&self, context: &mut super::cli_config::cli_context::CliContext) {
        context.local_runner_id = Some(self.edge_id().to_owned());
        #[cfg(unix)]
        {
            context.local_runner_attachment = Some(self.attachment().clone());
            context.local_runner_liveness = Some(self.client.liveness());
        }
    }
}

pub(crate) fn has_attachment(
    context: &super::cli_config::cli_context::CliContext,
    scope: &astra_credentials::LocalModelScope,
) -> bool {
    #[cfg(unix)]
    {
        context
            .local_runner_attachment
            .as_ref()
            .is_some_and(|attachment| attachment.belongs_to(scope))
            && context
                .local_runner_liveness
                .as_ref()
                .is_some_and(|liveness| liveness.is_alive())
    }
    #[cfg(not(unix))]
    {
        let _ = (context, scope);
        false
    }
}

fn runner_binary(current_exe: &Path) -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("ASTRA_EDGE_BIN") {
        return Ok(PathBuf::from(explicit));
    }
    current_exe
        .parent()
        .map(|parent| {
            parent.join(if cfg!(windows) {
                "astra-edge.exe"
            } else {
                "astra-edge"
            })
        })
        .ok_or_else(|| "Cannot locate the Astra installation directory".to_string())
}

fn configure_managed_identity(command: &mut tokio::process::Command, profile: &str, owner: &str) {
    // Only explicit local-state coordinates cross process startup. Provider
    // variables are resolved by each CLI and sent over authenticated IPC, never
    // inherited from whichever terminal happened to start the host first.
    command.env_clear();
    for key in ["HOME", "ASTRA_CLI_CREDENTIALS_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    // The explicitly supported networking policy is shared for this host.
    // Attachments with different proxy/CA settings are rejected by private IPC,
    // never silently routed through the first terminal's network boundary.
    for key in astra_core::net::RUNNER_NETWORK_ENV_VARS {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .arg("--profile")
        .arg(profile)
        .arg("--expected-inference-owner")
        .arg(owner)
        .env_remove("ASTRA_TOKEN")
        .env_remove("ASTRA_TOKEN_FILE")
        .env_remove("ASTRA_TOKEN_RENEW_URL");
}

pub(crate) fn offering_for_model(
    context: &super::cli_config::cli_context::CliContext,
    scope: &astra_credentials::LocalModelScope,
    name: &str,
) -> Result<String, String> {
    #[cfg(unix)]
    {
        use astra_turn_types::runner_inference::{
            RunnerInferenceBindingIdentity, RunnerInferenceId,
        };
        let attachment = context
            .local_runner_attachment
            .as_ref()
            .ok_or("No live local model attachment; reopen Astra")?;
        let config = scope
            .models()
            .load()
            .map_err(|_| "Cannot read the saved model")?;
        let model = config
            .models
            .get(name)
            .ok_or("Saved local model no longer exists")?;
        let client = matches!(
            model.credential,
            astra_credentials::LocalCredentialRef::Environment { .. }
        )
        .then_some(attachment.lease_id.as_str());
        let id = |value: String| {
            RunnerInferenceId::new(value).map_err(|_| "Invalid local model identity")
        };
        let identity = RunnerInferenceBindingIdentity {
            runner_id: id(attachment.runner_id.clone())?,
            journal_id: id(attachment.journal_id.clone())?,
            binding_id: id(astra_edge::inference_host::local_binding_id(name, client))?,
            // Offering identity deliberately excludes mutable revisions.
            binding_revision: std::num::NonZeroU64::MIN,
            profile_revision: std::num::NonZeroU64::MIN,
        };
        Ok(astra_services::runner_model_bindings::runner_offering_id(
            scope.account_id(),
            &identity,
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (context, scope, name);
        Err("Local model attachments require Linux or macOS".into())
    }
}

pub(crate) async fn start(
    api_origin: &str,
    profile: Option<&str>,
    _workspace: &Path,
) -> Result<ManagedLocalRunner, String> {
    #[cfg(not(unix))]
    {
        let _ = (api_origin, profile);
        Err("Managed local inference requires Linux or macOS".into())
    }
    #[cfg(unix)]
    {
        let requested_profile = profile.map(str::to_owned);
        let credentials = astra_credentials::CredentialStore::new()
            .load()
            .map_err(|_| "Cannot read the selected Astra profile for local inference")?;
        let profile = astra_credentials::CredentialStore::resolve_profile_name(
            profile,
            credentials.current_profile.as_deref(),
        );
        let scope = astra_credentials::LocalModelScope::for_profile(api_origin, Some(&profile))?;
        let task_scope = scope.clone();
        let installation = tokio::task::spawn_blocking(move || Installation::open(&task_scope))
            .await
            .map_err(|_| "Cannot inspect the local model host")?
            .map_err(|error| error.to_string())?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
        let first_connection = tokio::time::timeout(
            Duration::from_millis(500),
            ManagedClient::connect(
                &installation,
                scope.clone(),
                api_origin,
                requested_profile.as_deref(),
            ),
        )
        .await
        .unwrap_or(Err(
            astra_edge::inference_host::InferenceHostError::BindingUnavailable,
        ));
        match first_connection {
            Ok(client) => return Ok(ManagedLocalRunner { client }),
            Err(
                error @ (astra_edge::inference_host::InferenceHostError::OwnerMismatch
                | astra_edge::inference_host::InferenceHostError::UnsafeStorage
                | astra_edge::inference_host::InferenceHostError::NetworkPolicyMismatch
                | astra_edge::inference_host::InferenceHostError::LocalProtocolMismatch),
            ) => return Err(error.to_string()),
            Err(_) => {}
        }
        let executable =
            runner_binary(&std::env::current_exe().map_err(|_| "Cannot locate Astra")?)?;
        if !executable.is_file() {
            return Err(
                "Local model Runner is missing. Reinstall Astra or set ASTRA_EDGE_BIN.".into(),
            );
        }
        let mut command = tokio::process::Command::new(executable);
        configure_managed_identity(&mut command, &profile, scope.account_id());
        command
            .arg("--inference-only")
            .arg("--managed-inference-host")
            .arg("--server-url")
            .arg(api_origin)
            .arg("--reconnect=true")
            .current_dir(scope.root())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false);
        // A shared host must not belong to the first terminal's foreground
        // process group or controlling session. setsid is async-signal-safe;
        // no allocation, logging, or Rust locks run after fork.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|_| "Cannot start the local model host")?;
        // Reap only; no launcher owns the lifetime. Concurrent cold-start losers
        // exit under the shared process lock and attach to the same winner.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        loop {
            let connection = tokio::time::timeout_at(
                deadline,
                ManagedClient::connect(
                    &installation,
                    scope.clone(),
                    api_origin,
                    requested_profile.as_deref(),
                ),
            )
            .await
            .unwrap_or(Err(
                astra_edge::inference_host::InferenceHostError::BindingUnavailable,
            ));
            match connection {
                Ok(client) => return Ok(ManagedLocalRunner { client }),
                Err(
                    astra_edge::inference_host::InferenceHostError::OwnerMismatch
                    | astra_edge::inference_host::InferenceHostError::UnsafeStorage,
                ) => {
                    return Err("Local model host identity or socket permissions are invalid. Inspect the local installation; no takeover was attempted.".into());
                }
                Err(
                    error @ (astra_edge::inference_host::InferenceHostError::NetworkPolicyMismatch
                    | astra_edge::inference_host::InferenceHostError::LocalProtocolMismatch),
                ) => return Err(error.to_string()),
                Err(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let hint = installation.status_hint().unwrap_or_else(|| {
                    "Check Astra login, local state permissions, and Server connectivity".into()
                });
                return Err(format!(
                    "Local model host did not become ready. {hint}. No provider test was run; work remains readable."
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn matching_runner_identity_without_a_live_connection_is_not_attached() {
        let scope = astra_credentials::LocalModelScope::for_owner(
            "https://fixture.invalid",
            "fixture-owner",
        )
        .unwrap();
        let context = super::super::cli_config::cli_context::CliContext {
            local_runner_attachment: Some(
                serde_json::from_value(serde_json::json!({
                    "runner_id": "fixture-runner", "journal_id": "fixture-journal",
                    "lease_id": uuid::Uuid::new_v4().to_string(),
                    "scope": scope.identity(), "version": 1,
                }))
                .unwrap(),
            ),
            local_runner_liveness: Some(Default::default()),
            ..Default::default()
        };
        assert!(
            !has_attachment(&context, &scope),
            "a failed lease must allow explicit setup to reconnect"
        );
    }

    #[test]
    fn managed_runner_identity_ignores_unrelated_terminal_authentication() {
        let mut command = tokio::process::Command::new("astra-edge");
        for key in ["ASTRA_TOKEN", "ASTRA_TOKEN_FILE", "ASTRA_TOKEN_RENEW_URL"] {
            command.env(key, "unrelated-fixture-value");
        }
        configure_managed_identity(&mut command, "selected-profile", "selected-account");
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "--profile",
                "selected-profile",
                "--expected-inference-owner",
                "selected-account"
            ]
        );
        for key in ["ASTRA_TOKEN", "ASTRA_TOKEN_FILE", "ASTRA_TOKEN_RENEW_URL"] {
            assert!(
                !command
                    .as_std()
                    .get_envs()
                    .any(|(name, value)| name == key && value.is_some())
            );
        }
    }

    #[test]
    fn sibling_binary_path_is_platform_specific_and_not_shell_interpreted() {
        let path = runner_binary(Path::new("/opt/astra/bin/astra")).unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(if cfg!(windows) {
                "astra-edge.exe"
            } else {
                "astra-edge"
            })
        );
    }
}
