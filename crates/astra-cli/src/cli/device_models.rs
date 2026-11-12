//! Native model setup adapter. NativeStore owns platform credentials, the
//! device ChatGPT owner performs OpenAI I/O, and Server admits every Offering.
use astra_credentials::{
    LocalChatgptModel, LocalModelScope,
    chatgpt::{Connection, Store},
    native::NativeSession,
    native_device::NativeDeviceAttachment,
};
use astra_edge::local_host::{Installation, ManagedClient};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct DeviceModels {
    binding: Arc<super::native_auth::Binding>,
    session: NativeSession,
    scope: LocalModelScope,
    reference: Option<NativeDeviceAttachment>,
}

pub(crate) struct DeviceRunner {
    client: ManagedClient,
}

impl DeviceModels {
    pub(crate) async fn current(binding: Arc<super::native_auth::Binding>) -> Result<Self, String> {
        binding
            .access_token()
            .await
            .map_err(|_| "Renew your MOI sign-in with /login")?;
        let session = binding.snapshot_off_runtime().await?;
        let scope = LocalModelScope::for_native_session(&session)?;
        let reference = if session.workspace_id.is_some() {
            let store = binding.store();
            let session = session.clone();
            Some(
                tokio::task::spawn_blocking(move || NativeDeviceAttachment::new(&store, &session))
                    .await
                    .map_err(|_| "Cannot inspect MOI device authorization")??,
            )
        } else {
            None
        };
        Ok(Self {
            binding,
            session,
            scope,
            reference,
        })
    }

    async fn admit_source(&self) -> Result<(), String> {
        let binding = &self.binding;
        let session = &self.session;
        // Platform audience exchange uses the existing native credential owner.
        // Device-source admission never changes account-level model policy.
        let credential = binding
            .store()
            .credential("moi", Some(&session.generation))
            .await
            .map_err(|_| "Renew your MOI sign-in with /login")?;
        if credential.endpoint != session.environment.moi_url
            || credential.subject != session.subject
        {
            return Err("MOI model source owner changed".into());
        }
        let mut url = url::Url::parse(&credential.endpoint).map_err(|_| "Invalid MOI origin")?;
        let workspace = session
            .workspace_id
            .as_deref()
            .ok_or("Select a MOI workspace before connecting ChatGPT")?;
        url.path_segments_mut()
            .map_err(|_| "Invalid MOI origin")?
            .pop_if_empty()
            .extend([
                "workspaces",
                workspace,
                "model-sources",
                "chatgpt-device",
                "admission",
            ]);
        let mut request = astra_credentials::native::http_client()?
            .get(url)
            .bearer_auth(credential.access_token)
            .header("X-Workspace-ID", workspace)
            .header("Accept-Language", "en-US");
        if let Some(role) = &session.role_id {
            request = request.header("X-MOI-Role-ID", role);
        }
        let admission = tokio::time::timeout(Duration::from_secs(15), async {
            let mut response = request
                .send()
                .await
                .map_err(|_| "MOI model source admission is unavailable")?;
            let status = response.status();
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "MOI model source admission is unavailable")?
            {
                if chunk.len() > (64 * 1024usize).saturating_sub(bytes.len()) {
                    return Err("Invalid MOI model source response".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| "Invalid MOI model source response")?;
            if status != reqwest::StatusCode::OK {
                return Err(super::cli_config::cli_utils::read_api_error(
                    status.as_u16(),
                    &String::from_utf8_lossy(&bytes),
                ));
            }
            if value["code"] != "OK"
                || value["data"]["workspace_id"] != workspace
                || value["data"]["source"] != "chatgpt_device"
                || !value["data"]["policy_revision"]
                    .as_str()
                    .is_some_and(|revision| revision.starts_with("sha256:"))
            {
                return Err(
                    "MOI model source admission did not match this workspace and account".into(),
                );
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|_| "MOI model source admission timed out")?;
        admission?;
        Ok(())
    }

    pub(crate) async fn connections(&self) -> Result<Vec<Connection>, String> {
        let store = Store::new(&self.scope);
        tokio::task::spawn_blocking(move || store.list())
            .await
            .map_err(|_| "Cannot read device connections")?
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn models(&self, id: &str) -> Result<Vec<String>, String> {
        self.admit_source().await?;
        astra_edge::chatgpt::Client::new()
            .map_err(|error| error.to_string())?
            .catalog(Store::new(&self.scope), parse_id(id)?)
            .await
            .map(|items| items.into_iter().map(|model| model.slug).collect())
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn connect(
        &self,
        existing: Option<String>,
        label: Option<String>,
        cancel: CancellationToken,
        mut progress: impl FnMut(super::model_connections::ConnectProgress) -> Result<(), String>,
    ) -> Result<Connection, String> {
        self.admit_source().await?;
        if label.as_ref().is_some_and(|value| {
            value.is_empty() || value.len() > 128 || value.chars().any(char::is_control)
        }) {
            return Err("Choose a nonempty connection label of at most 128 bytes".into());
        }
        let reference = self
            .reference
            .clone()
            .ok_or("Select a MOI workspace before connecting ChatGPT")?;
        let id = existing
            .as_deref()
            .map(parse_id)
            .transpose()?
            .unwrap_or_else(Uuid::new_v4);
        let previous = self
            .connections()
            .await?
            .into_iter()
            .find(|connection| connection.connection_ref == id);
        if existing.is_some() && previous.is_none() {
            return Err("Device ChatGPT connection no longer exists".into());
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| "Cannot listen for ChatGPT authorization")?;
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener
                .local_addr()
                .map_err(|_| "Cannot read callback port")?
                .port()
        );
        let store = Store::new(&self.scope);
        let host_id = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || store.host_id())
                .await
                .map_err(|_| "Cannot read ChatGPT registration")?
                .map_err(|error| error.to_string())?
        };
        let client = astra_edge::chatgpt::Client::new().map_err(|error| error.to_string())?;
        let mut request = client
            .begin(
                host_id,
                previous.as_ref().map(|connection| &connection.registration),
                redirect,
            )
            .map_err(|error| error.to_string())?;
        request.consent = previous
            .as_ref()
            .is_some_and(|connection| !connection.enabled);
        let url = request.authorization_url(
            url::Url::parse(astra_credentials::chatgpt::protocol::AUTHORIZATION_ENDPOINT)
                .expect("fixed official authorization URL"),
        );
        progress(super::model_connections::ConnectProgress::Waiting(
            url.to_string(),
        ))?;
        let callback = super::model_connections::wait_for_browser_callback(
            listener,
            request.state.clone(),
            url.as_str(),
            cancel,
            &mut progress,
        )
        .await?;
        progress(super::model_connections::ConnectProgress::Completing)?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(
                callback
                    .as_object()
                    .ok_or("Invalid ChatGPT callback")?
                    .iter()
                    .filter_map(|(key, value)| value.as_str().map(|value| (key, value))),
            )
            .finish();
        // Consuming a one-use code owns settlement even if the view closes.
        let device = self.clone();
        tokio::spawn(async move {
            let grant = client
                .exchange(
                    &request,
                    &query,
                    previous.as_ref().map(|connection| &connection.registration),
                )
                .await
                .map_err(|error| error.to_string())?;
            client
                .publish_native(
                    grant,
                    reference,
                    device.scope.clone(),
                    id,
                    previous
                        .as_ref()
                        .map(|connection| connection.authorization_revision),
                    label.unwrap_or_else(|| {
                        previous.map_or_else(|| "My ChatGPT".into(), |connection| connection.label)
                    }),
                )
                .await
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| "ChatGPT authorization settlement did not finish; reopen /model".to_owned())?
    }

    pub(crate) async fn saved(&self) -> Result<Vec<(String, LocalChatgptModel)>, String> {
        let models = self.scope.models();
        tokio::task::spawn_blocking(move || models.load())
            .await
            .map_err(|_| "Cannot read device models")?
            .map(|config| config.chatgpt_models.into_iter().collect())
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn save(
        &self,
        name: String,
        connection: String,
        model: String,
        context_window: u32,
        output_limit: u32,
        update: bool,
    ) -> Result<(String, DeviceRunner), String> {
        self.admit_source().await?;
        if context_window == 0
            || output_limit == 0
            || output_limit > context_window
            || context_window > i32::MAX as u32
        {
            return Err(
                "Choose positive context and output limits; output must fit the context".into(),
            );
        }
        let id = parse_id(&connection)?;
        let efforts = astra_edge::chatgpt::Client::new()
            .map_err(|error| error.to_string())?
            .probe(Store::new(&self.scope), id, &model, output_limit as usize)
            .await
            .map_err(|error| error.to_string())?;
        let connection = self
            .connections()
            .await?
            .into_iter()
            .find(|connection| connection.connection_ref == id && connection.enabled)
            .ok_or("Authorize this ChatGPT connection from /model first")?;
        let local = self.scope.models();
        let reference = self
            .reference
            .clone()
            .ok_or("Select a MOI workspace before enabling device ChatGPT")?;
        let scope = self.scope.clone();
        let revision = connection.authorization_revision;
        let saved_name = name.clone();
        let native_store = self.binding.store();
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            let _lease = native_store.device_model_lease(&session)?;
            if !reference.matches_scope(&scope) {
                return Err("MOI account changed during model setup".into());
            }
            let mut config = local.load().map_err(|error| error.to_string())?;
            if config.models.contains_key(&name)
                || (!update && config.chatgpt_models.contains_key(&name))
            {
                return Err("A device model with this name already exists".into());
            }
            if update && !config.chatgpt_models.contains_key(&name) {
                return Err("Device model no longer exists".into());
            }
            let binding_revision = config
                .chatgpt_models
                .get(&name)
                .map_or(1, |model| model.binding_revision);
            config.chatgpt_models.insert(
                name,
                LocalChatgptModel {
                    connection_ref: id,
                    authorization_revision: revision,
                    model,
                    binding_revision,
                    context_window,
                    max_output_tokens: output_limit,
                    supported_reasoning_efforts: efforts,
                },
            );
            local
                .replace(config.revision, config)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|_| "Device model publication did not finish")??;
        // If login changed after the config commit, consent is rejected and the
        // saved definition stays inactive. It never resurrects an old session.
        let store = self.binding.store();
        let expected = self.session.clone();
        let consent = tokio::task::spawn_blocking(move || {
            store.enable_device_connection(&expected, id, revision)
        })
        .await
        .map_err(|_| "Model saved; device authorization did not finish. Reopen /model")??;
        if !consent.directory_synced {
            return Err("Model consent is visible but durability is unconfirmed. Reopen /model before using it".into());
        }
        let runner = start(self.binding.clone()).await?;
        let reference = self.reference.as_ref().ok_or("MOI workspace is required")?;
        let offering = offering(&runner, &self.scope, reference, &saved_name)?;
        Ok((offering, runner))
    }

    pub(crate) async fn remove(&self, name: String) -> Result<(), String> {
        let local = self.scope.models();
        let store = self.binding.store();
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            let _lease = store.device_model_lease(&session)?;
            let mut config = local.load().map_err(|error| error.to_string())?;
            config
                .chatgpt_models
                .remove(&name)
                .ok_or("Device model no longer exists")?;
            local
                .replace(config.revision, config)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|_| "Device model removal did not finish")?
    }

    pub(crate) async fn disconnect(&self, id: &str) -> Result<bool, String> {
        astra_edge::chatgpt::Client::disconnect_default(Store::new(&self.scope), parse_id(id)?)
            .await
            .map_err(|error| error.to_string())
    }
}

fn parse_id(id: &str) -> Result<Uuid, String> {
    Uuid::parse_str(id).map_err(|_| "Use the exact device ChatGPT connection identity".into())
}

fn offering(
    runner: &DeviceRunner,
    scope: &LocalModelScope,
    reference: &NativeDeviceAttachment,
    name: &str,
) -> Result<String, String> {
    use astra_turn_types::runner_inference::{RunnerInferenceBindingIdentity, RunnerInferenceId};
    let id = |value| {
        RunnerInferenceId::new(value).map_err(|_| "Invalid local model attachment".to_owned())
    };
    let attachment = &runner.client.attachment;
    let binding = RunnerInferenceBindingIdentity {
        runner_id: id(attachment.runner_id.clone())?,
        journal_id: id(attachment.journal_id.clone())?,
        binding_id: id(astra_edge::inference_host::native_binding_id(
            name,
            &reference.binding_key(),
        ))?,
        binding_revision: std::num::NonZeroU64::MIN,
        profile_revision: std::num::NonZeroU64::MIN,
    };
    Ok(astra_services::runner_model_bindings::runner_offering_id(
        scope.account_id(),
        &binding,
    ))
}

pub(crate) async fn start_if_configured() -> Result<Option<DeviceRunner>, String> {
    let Some(binding) = super::native_auth::active() else {
        return Ok(None);
    };
    let current = binding.snapshot_off_runtime().await?;
    if current.workspace_id.is_none() {
        return Ok(None);
    }
    let scope = LocalModelScope::for_native_session(&current)?;
    let local = scope.models();
    let store = binding.store();
    let enabled = tokio::task::spawn_blocking(move || {
        let config = local.load().map_err(|error| error.to_string())?;
        for model in config.chatgpt_models.values() {
            if store.device_connection_enabled(
                &current,
                model.connection_ref,
                model.authorization_revision,
            )? {
                return Ok::<_, String>(true);
            }
        }
        Ok(false)
    })
    .await
    .map_err(|_| "Cannot inspect device models")??;
    if enabled {
        start(binding).await.map(Some)
    } else {
        Ok(None)
    }
}

async fn start(binding: Arc<super::native_auth::Binding>) -> Result<DeviceRunner, String> {
    let session = binding.snapshot_off_runtime().await?;
    let scope = LocalModelScope::for_native_session(&session)?;
    let store = binding.store();
    let current = session.clone();
    let reference =
        tokio::task::spawn_blocking(move || NativeDeviceAttachment::new(&store, &current))
            .await
            .map_err(|_| "Cannot inspect device authorization")??;
    let installation = {
        let scope = scope.clone();
        tokio::task::spawn_blocking(move || Installation::open(&scope))
            .await
            .map_err(|_| "Cannot inspect the device host")?
            .map_err(|error| error.to_string())?
    };
    match tokio::time::timeout(
        Duration::from_millis(500),
        ManagedClient::connect_native(&installation, scope.clone(), reference.clone()),
    )
    .await
    .unwrap_or(Err(
        astra_edge::inference_host::InferenceHostError::BindingUnavailable,
    )) {
        Ok(client) => return Ok(DeviceRunner { client }),
        Err(error) if !host_may_be_starting(error) => return Err(error.to_string()),
        Err(_) => {}
    }
    let executable = std::env::var_os("ASTRA_EDGE_BIN")
        .map(PathBuf::from)
        .unwrap_or(
            std::env::current_exe()
                .map_err(|_| "Cannot locate the Astra installation")?
                .with_file_name("astra-edge"),
        );
    if !executable.is_file() {
        return Err(
            "Install matching Astra and astra-edge binaries before using device ChatGPT".into(),
        );
    }
    let mut command = tokio::process::Command::new(executable);
    command.env_clear();
    for key in ["HOME", "ASTRA_CLI_CREDENTIALS_DIR"]
        .into_iter()
        .chain(astra_core::net::RUNNER_NETWORK_ENV_VARS.iter().copied())
    {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .arg("--inference-only")
        .arg("--native-owner-dir")
        .arg(binding.store().directory())
        .arg("--expected-inference-owner")
        .arg(&session.astra_user_id)
        .arg("--server-url")
        .arg(&session.environment.astra_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
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
        .map_err(|_| "Cannot start the device model host")?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    loop {
        match tokio::time::timeout_at(
            deadline,
            ManagedClient::connect_native(&installation, scope.clone(), reference.clone()),
        )
        .await
        {
            Ok(Ok(client)) => return Ok(DeviceRunner { client }),
            Ok(Err(error)) if !host_may_be_starting(error) => return Err(error.to_string()),
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Device model host did not become ready; check the matching astra-edge installation".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn host_may_be_starting(error: astra_edge::inference_host::InferenceHostError) -> bool {
    use astra_edge::inference_host::InferenceHostError;
    matches!(
        error,
        InferenceHostError::BindingUnavailable | InferenceHostError::JournalIo
    )
}
