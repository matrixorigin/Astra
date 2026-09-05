use std::io::IsTerminal;

use astra_credentials::{
    LocalCredentialRef, LocalInferenceProtocol, LocalModelConfigStore, LocalModelDefinition,
    LocalSecretStore, ResolvedLocalCredential,
};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cli::cli_config::cli_args::{ModelAddArgs, ModelCheckArgs, ModelRemoveArgs};
use crate::cli::cli_config::cli_utils::prompt_or;

#[derive(Serialize)]
struct LocalModelStatus<'a> {
    name: &'a str,
    source: &'static str,
    configuration: &'static str,
    credential: &'static str,
    provider_probe: &'static str,
    config_path: String,
}

pub(crate) fn add(args: ModelAddArgs) -> Result<String, String> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let name = required("model name", args.name, interactive)?;
    let base_url = required("API base URL", args.base_url, interactive)?;
    let provider_model = required("Provider model", args.provider_model, interactive)?;
    let context_window = required_u32("Context window", args.context_window, interactive)?;
    let max_output_tokens =
        required_u32("Maximum output tokens", args.max_output_tokens, interactive)?;

    let store = LocalModelConfigStore::new();
    let secrets = LocalSecretStore::new();
    let mut created_secret = None;
    let credential = if let Some(name) = args.credential_env {
        LocalCredentialRef::Environment { name }
    } else if args.no_auth {
        LocalCredentialRef::None
    } else if args.store_secret {
        if !interactive {
            return Err(
                "--store-secret requires an interactive terminal; use --credential-env for automation"
                    .to_string(),
            );
        }
        let secret_id = store_secret(&secrets)?;
        created_secret = Some(secret_id.clone());
        LocalCredentialRef::ProtectedFile { secret_id }
    } else if interactive {
        let source = prompt_or("Credential source (environment, stored, or none)", None)?;
        match source.trim().to_ascii_lowercase().as_str() {
            "environment" | "env" => LocalCredentialRef::Environment {
                name: prompt_or("Environment variable", None)?,
            },
            "stored" | "file" => {
                let secret_id = store_secret(&secrets)?;
                created_secret = Some(secret_id.clone());
                LocalCredentialRef::ProtectedFile { secret_id }
            }
            "none" | "keyless" => LocalCredentialRef::None,
            _ => {
                return Err("credential source must be environment, stored, or none".to_string());
            }
        }
    } else {
        return Err(
            "non-interactive setup requires one of --credential-env, --store-secret, or --no-auth"
                .to_string(),
        );
    };

    save_definition(
        &store,
        &secrets,
        LocalModelDefinitionInput {
            name,
            definition: LocalModelDefinition {
                protocol: LocalInferenceProtocol::OpenaiCompatible,
                base_url,
                model: provider_model,
                binding_revision: 0,
                context_window,
                max_output_tokens,
                credential,
            },
        },
        created_secret,
    )
}

pub(crate) enum LocalModelCredentialInput {
    Environment(String),
    Stored(String),
    None,
}

pub(crate) fn add_from_tui(
    name: String,
    base_url: String,
    provider_model: String,
    context_window: u32,
    max_output_tokens: u32,
    credential_input: LocalModelCredentialInput,
) -> Result<String, String> {
    let store = LocalModelConfigStore::new();
    let secrets = LocalSecretStore::new();
    let (credential, created_secret) = match credential_input {
        LocalModelCredentialInput::Environment(name) => {
            (LocalCredentialRef::Environment { name }, None)
        }
        LocalModelCredentialInput::Stored(secret) => {
            let secret_id = format!("model_{}", uuid::Uuid::new_v4().simple());
            secrets
                .put(&secret_id, &secret)
                .map_err(|error| error.to_string())?;
            (
                LocalCredentialRef::ProtectedFile {
                    secret_id: secret_id.clone(),
                },
                Some(secret_id),
            )
        }
        LocalModelCredentialInput::None => (LocalCredentialRef::None, None),
    };
    save_definition(
        &store,
        &secrets,
        LocalModelDefinitionInput {
            name,
            definition: LocalModelDefinition {
                protocol: LocalInferenceProtocol::OpenaiCompatible,
                base_url,
                model: provider_model,
                binding_revision: 0,
                context_window,
                max_output_tokens,
                credential,
            },
        },
        created_secret,
    )
}

struct LocalModelDefinitionInput {
    name: String,
    definition: LocalModelDefinition,
}

fn save_definition(
    store: &LocalModelConfigStore,
    secrets: &LocalSecretStore,
    input: LocalModelDefinitionInput,
    created_secret: Option<String>,
) -> Result<String, String> {
    let LocalModelDefinitionInput { name, definition } = input;
    let mut config = store.load().map_err(|error| error.to_string())?;
    let mut definition = definition;
    definition.binding_revision = config.models.get(&name).map_or(Ok(1), |previous| {
        previous.binding_revision.checked_add(1).ok_or_else(|| {
            "local model binding revision is exhausted; remove and recreate the model".to_string()
        })
    })?;
    let previous = config.models.insert(name.clone(), definition);
    let expected_revision = config.revision;
    let applied = match store.replace(expected_revision, config) {
        Ok(applied) => applied,
        Err(error) => {
            if let Some(secret_id) = created_secret.as_deref() {
                let _ = secrets.remove(secret_id);
            }
            return Err(error.to_string());
        }
    };

    if let Some(LocalCredentialRef::ProtectedFile { secret_id }) =
        previous.as_ref().map(|definition| &definition.credential)
    {
        if Some(secret_id.as_str()) != created_secret.as_deref() {
            let _ = secrets.remove(secret_id);
        }
    }
    let next = format!("astra model check {name}");
    serde_json::to_string_pretty(&serde_json::json!({
        "name": name,
        "status": "saved_locally",
        "revision": applied.revision,
        "binding_revision": applied
            .models
            .get(&name)
            .map(|definition| definition.binding_revision)
            .unwrap_or_default(),
        "provider_probe": "not_run",
        "config_path": store.path(),
        "next": next,
    }))
    .map_err(|error| error.to_string())
}

fn required_u32(label: &'static str, value: Option<u32>, interactive: bool) -> Result<u32, String> {
    if let Some(value) = value {
        return (value > 0)
            .then_some(value)
            .ok_or_else(|| format!("{label} must be greater than zero"));
    }
    if !interactive {
        return Err(format!("{label} is required in non-interactive mode"));
    }
    let value = prompt_or(label, None)?;
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{label} must be a positive integer"))
}

pub(crate) async fn check(args: ModelCheckArgs) -> Result<String, String> {
    let store = LocalModelConfigStore::new();
    let config = store.load().map_err(|error| error.to_string())?;
    let definition = config
        .models
        .get(&args.name)
        .ok_or_else(|| format!("local model '{}' is not configured", args.name))?;
    definition.validate().map_err(|error| error.to_string())?;
    let credential = match &definition.credential {
        LocalCredentialRef::Environment { .. } | LocalCredentialRef::None => {
            ResolvedLocalCredential::from_environment(&definition.credential, |name| {
                std::env::var(name).ok()
            })
        }
        LocalCredentialRef::ProtectedFile { .. } | LocalCredentialRef::SystemKeychain { .. } => {
            LocalSecretStore::new().resolve(&definition.credential)
        }
    }
    .map_err(|error| error.to_string())?;
    let api_key = credential
        .as_ref()
        .map(ResolvedLocalCredential::expose_to_local_transport)
        .unwrap_or("");
    let request = astra_inference_adapter::ExactProviderRequest::compile(
        &serde_json::json!({
            "model": definition.model,
            "messages": [{"role": "user", "content": "Reply with OK."}],
            "max_tokens": definition.max_output_tokens.min(4),
            "stream": true,
        }),
        astra_inference_adapter::ProviderProtocol::OpenAiCompatible,
        64 * 1024,
    )
    .map_err(|error| error.to_string())?;
    let endpoint = chat_completions_endpoint(&definition.base_url)?;
    let transport = astra_inference_adapter::transport::ProviderTransport::build(
        astra_core::net::runner_provider_client_builder().map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let headers = astra_inference_adapter::transport::provider_headers(
        astra_inference_adapter::ProviderProtocol::OpenAiCompatible,
        api_key,
        std::iter::empty::<(&str, &str)>(),
    )
    .map_err(|error| error.to_string())?;
    let attempt = transport
        .prepare(
            &endpoint,
            headers,
            &request,
            Some(std::time::Duration::from_secs(20)),
        )
        .map_err(|error| error.to_string())?;
    let (events_tx, mut events_rx) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let execute = async move {
        transport
            .execute(
                attempt,
                astra_inference_adapter::transport::ResponseMode::Sse,
                astra_inference_adapter::transport::ExecutionLimits {
                    event_bytes: 256 * 1024,
                    total_bytes: 1024 * 1024,
                    events: 128,
                },
                deadline,
                &cancellation,
                &events_tx,
            )
            .await
    };
    let drain = async {
        let mut json_events = 0_u64;
        let mut saw_choice = false;
        let mut saw_provider_error = false;
        let mut done = false;
        while let Some(event) = events_rx.recv().await {
            match event {
                astra_inference_adapter::transport::ProviderEvent::Json(value) => {
                    json_events += 1;
                    saw_choice |= value
                        .get("choices")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|choices| !choices.is_empty());
                    saw_provider_error |= value.get("error").is_some();
                }
                astra_inference_adapter::transport::ProviderEvent::Done => done = true,
                astra_inference_adapter::transport::ProviderEvent::Eof => {}
            }
        }
        (json_events, saw_choice, saw_provider_error, done)
    };
    let (terminal, (json_events, saw_choice, saw_provider_error, done)) =
        tokio::join!(execute, drain);
    if terminal.status != astra_inference_adapter::transport::ExecutionStatus::Complete
        || json_events == 0
        || !saw_choice
        || saw_provider_error
    {
        return Err(format!(
            "provider probe failed ({:?}); configuration remains saved and no retry was attempted",
            terminal.status
        ));
    }
    drop(credential);
    serde_json::to_string_pretty(&LocalModelStatus {
        name: &args.name,
        source: credential_kind(&definition.credential),
        configuration: "valid",
        credential: "available",
        provider_probe: if done {
            "stream_verified"
        } else {
            "stream_eof_verified"
        },
        config_path: store.path().display().to_string(),
    })
    .map_err(|error| error.to_string())
}

fn chat_completions_endpoint(base_url: &str) -> Result<String, String> {
    let mut base = url::Url::parse(base_url).map_err(|_| "invalid local model base URL")?;
    if base
        .path()
        .trim_end_matches('/')
        .ends_with("/chat/completions")
    {
        return Ok(base.to_string());
    }
    let path = format!("{}/chat/completions", base.path().trim_end_matches('/'));
    base.set_path(&path);
    Ok(base.to_string())
}

pub(crate) fn show(name: &str) -> Result<Option<String>, String> {
    let store = LocalModelConfigStore::new();
    let config = store.load().map_err(|error| error.to_string())?;
    let Some(definition) = config.models.get(name) else {
        return Ok(None);
    };
    serde_json::to_string_pretty(&serde_json::json!({
        "name": name,
        "scope": "runner_local",
        "protocol": definition.protocol,
        "base_url": definition.base_url,
        "model": definition.model,
        "binding_revision": definition.binding_revision,
        "context_window": definition.context_window,
        "max_output_tokens": definition.max_output_tokens,
        "credential_source": credential_kind(&definition.credential),
        "credential_storage": credential_storage(&definition.credential),
        "revision": config.revision,
        "config_path": store.path(),
    }))
    .map(Some)
    .map_err(|error| error.to_string())
}

pub(crate) fn remove(args: ModelRemoveArgs) -> Result<String, String> {
    let store = LocalModelConfigStore::new();
    let secrets = LocalSecretStore::new();
    let mut config = store.load().map_err(|error| error.to_string())?;
    let removed = config
        .models
        .remove(&args.name)
        .ok_or_else(|| format!("local model '{}' is not configured", args.name))?;
    let expected_revision = config.revision;
    let applied = store
        .replace(expected_revision, config)
        .map_err(|error| error.to_string())?;
    let cleanup = if let LocalCredentialRef::ProtectedFile { secret_id } = removed.credential {
        match secrets.remove(&secret_id) {
            Ok(_) => "complete",
            Err(_) => "credential_cleanup_required",
        }
    } else {
        "not_applicable"
    };
    serde_json::to_string_pretty(&serde_json::json!({
        "name": args.name,
        "status": "removed_locally",
        "revision": applied.revision,
        "credential_cleanup": cleanup,
    }))
    .map_err(|error| error.to_string())
}

fn required(label: &str, value: Option<String>, interactive: bool) -> Result<String, String> {
    if value.is_some() || interactive {
        prompt_or(label, value)
    } else {
        Err(format!("{label} is required in non-interactive setup"))
    }
}

fn store_secret(store: &LocalSecretStore) -> Result<String, String> {
    use std::io::Write;

    eprint!("  Provider API key: ");
    std::io::stderr()
        .flush()
        .map_err(|error| error.to_string())?;
    let value = rpassword::read_password().map_err(|error| error.to_string())?;
    if value.is_empty() {
        return Err("Provider API key cannot be empty".to_string());
    }
    let secret_id = format!("model_{}", uuid::Uuid::new_v4().simple());
    store
        .put(&secret_id, &value)
        .map_err(|error| error.to_string())?;
    Ok(secret_id)
}

fn credential_kind(reference: &LocalCredentialRef) -> &'static str {
    match reference {
        LocalCredentialRef::Environment { .. } => "environment",
        LocalCredentialRef::ProtectedFile { .. } => "protected_file",
        LocalCredentialRef::SystemKeychain { .. } => "system_keychain",
        LocalCredentialRef::None => "none",
    }
}

fn credential_storage(reference: &LocalCredentialRef) -> &'static str {
    match reference {
        LocalCredentialRef::Environment { .. } => "process_environment",
        LocalCredentialRef::ProtectedFile { .. } => "owner_only_plaintext_file",
        LocalCredentialRef::SystemKeychain { .. } => "system_keychain_unavailable",
        LocalCredentialRef::None => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn no_auth_add(name: &str) -> ModelAddArgs {
        ModelAddArgs {
            name: Some(name.to_string()),
            base_url: Some("http://127.0.0.1:8080/v1".to_string()),
            provider_model: Some("coding-model".to_string()),
            context_window: Some(128_000),
            max_output_tokens: Some(8_192),
            credential_env: None,
            no_auth: true,
            store_secret: false,
        }
    }

    #[test]
    #[serial]
    fn local_model_lifecycle_is_offline_and_revisioned() {
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());

        let added = add(no_auth_add("work")).unwrap();
        assert!(added.contains("saved_locally"));
        let first: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert_eq!(first["binding_revision"], 1);

        add(no_auth_add("other")).unwrap();
        let other: serde_json::Value =
            serde_json::from_str(&show("other").unwrap().unwrap()).unwrap();
        assert_eq!(other["binding_revision"], 1);

        add(no_auth_add("work")).unwrap();
        let updated: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert_eq!(updated["binding_revision"], 2);
        let other_after: serde_json::Value =
            serde_json::from_str(&show("other").unwrap().unwrap()).unwrap();
        assert_eq!(other_after["binding_revision"], 1);

        let removed = remove(ModelRemoveArgs {
            name: "work".to_string(),
        })
        .unwrap();
        assert!(removed.contains("removed_locally"));
        assert!(show("work").unwrap().is_none());
    }

    #[test]
    fn provider_endpoint_is_derived_without_rewriting_query_or_duplicating_path() {
        assert_eq!(
            chat_completions_endpoint("https://provider.example/v1?api-version=1").unwrap(),
            "https://provider.example/v1/chat/completions?api-version=1"
        );
        assert_eq!(
            chat_completions_endpoint("https://provider.example/v1/chat/completions?api-version=1")
                .unwrap(),
            "https://provider.example/v1/chat/completions?api-version=1"
        );
    }

    #[tokio::test]
    #[serial]
    async fn check_performs_one_real_bounded_stream_probe() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        add(ModelAddArgs {
            base_url: Some(format!("{}/v1", server.uri())),
            ..no_auth_add("work")
        })
        .unwrap();

        let checked = check(ModelCheckArgs {
            name: "work".to_string(),
        })
        .await
        .unwrap();
        assert!(checked.contains("\"configuration\": \"valid\""));
        assert!(checked.contains("\"provider_probe\": \"stream_verified\""));
    }

    #[tokio::test]
    #[serial]
    async fn failed_probe_is_not_retried_and_preserves_saved_configuration() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        add(ModelAddArgs {
            base_url: Some(format!("{}/v1", server.uri())),
            ..no_auth_add("work")
        })
        .unwrap();

        let error = check(ModelCheckArgs {
            name: "work".to_string(),
        })
        .await
        .unwrap_err();
        assert!(error.contains("HttpStatus(401)"));
        assert!(error.contains("no retry was attempted"));
        assert!(show("work").unwrap().is_some());
    }

    #[tokio::test]
    #[serial]
    async fn provider_probe_never_forwards_authorization_across_redirects() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let redirect_target = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/capture"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&redirect_target)
            .await;
        let provider = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/capture", redirect_target.uri())),
            )
            .expect(1)
            .mount(&provider)
            .await;

        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        add_from_tui(
            "work".to_string(),
            format!("{}/v1", provider.uri()),
            "coding-model".to_string(),
            128_000,
            8_192,
            LocalModelCredentialInput::Stored("secret-canary".to_string()),
        )
        .unwrap();
        check(ModelCheckArgs {
            name: "work".to_string(),
        })
        .await
        .expect_err("redirect must not be followed");
    }

    #[tokio::test]
    #[serial]
    async fn provider_error_envelope_is_not_mistaken_for_a_successful_probe() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"error\":{\"message\":\"model unavailable\"}}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        add(ModelAddArgs {
            base_url: Some(format!("{}/v1", server.uri())),
            ..no_auth_add("work")
        })
        .unwrap();

        let error = check(ModelCheckArgs {
            name: "work".to_string(),
        })
        .await
        .unwrap_err();
        assert!(error.contains("provider probe failed"));
        assert!(show("work").unwrap().is_some());
    }

    #[tokio::test]
    #[serial]
    async fn empty_choice_stream_is_not_mistaken_for_a_successful_probe() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        add(ModelAddArgs {
            base_url: Some(format!("{}/v1", server.uri())),
            ..no_auth_add("work")
        })
        .unwrap();

        let error = check(ModelCheckArgs {
            name: "work".to_string(),
        })
        .await
        .unwrap_err();
        assert!(error.contains("provider probe failed"));
        assert!(show("work").unwrap().is_some());
    }

    #[test]
    fn noninteractive_setup_fails_before_writing_when_required_input_is_missing() {
        let error = add(ModelAddArgs {
            name: Some("work".to_string()),
            base_url: None,
            provider_model: None,
            context_window: None,
            max_output_tokens: None,
            credential_env: None,
            no_auth: true,
            store_secret: false,
        })
        .unwrap_err();
        assert!(error.contains("API base URL is required"));
    }
}
