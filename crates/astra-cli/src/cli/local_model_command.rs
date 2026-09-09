use std::io::IsTerminal;

use astra_credentials::{
    LocalCredentialRef, LocalInferenceProtocol, LocalModelConfigError, LocalModelConfigStore,
    LocalModelDefinition, LocalModelProbeState, LocalModelScope, LocalSecretStore,
    ResolvedLocalCredential,
};
use astra_inference_adapter::openai::chat_completions_endpoint;
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

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LocalModelStatusRow {
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) binding_revision: u64,
    pub(crate) credential_source: &'static str,
    pub(crate) credential: &'static str,
    pub(crate) provider_probe: &'static str,
    pub(crate) probe_checked_at_unix_ms: Option<u64>,
    pub(crate) probe_failure_code: Option<String>,
    pub(crate) status: &'static str,
    pub(crate) next: String,
}

/// The provider response and the non-secret identity of the credential that
/// was actually attached to that request. Keeping these together prevents a
/// terminal-local environment value from being detached from its evidence.
struct LocalProbeResult {
    body: String,
    credential_fingerprint: Option<String>,
}

struct LocalProbeError {
    message: String,
    credential_fingerprint: Option<String>,
}

impl LocalProbeError {
    fn new(message: impl Into<String>, credential_fingerprint: Option<String>) -> Self {
        Self {
            message: message.into(),
            credential_fingerprint,
        }
    }
}

impl std::fmt::Display for LocalProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub(crate) fn add(scope: &LocalModelScope, args: ModelAddArgs) -> Result<String, String> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let name = required("model name", args.name, interactive)?;
    let base_url = required("API base URL", args.base_url, interactive)?;
    let provider_model = required("Provider model", args.provider_model, interactive)?;
    let context_window = required_u32("Context window", args.context_window, interactive)?;
    let max_output_tokens =
        required_u32("Maximum output tokens", args.max_output_tokens, interactive)?;

    let store = scope.models();
    let secrets = scope.secrets();
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
                probe: LocalModelProbeState::default(),
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

/// Build and validate the user-facing local model fields through the same
/// durable configuration contract used by both the CLI and the TUI. The TUI
/// passes a placeholder protected-file reference for a secret that has not
/// been stored yet; this function performs no I/O and only checks shape and
/// bounds.
pub(crate) fn validated_local_model_definition(
    name: &str,
    base_url: &str,
    provider_model: &str,
    context_window: u32,
    max_output_tokens: u32,
    credential: &LocalCredentialRef,
) -> Result<LocalModelDefinition, LocalModelConfigError> {
    let definition = LocalModelDefinition {
        protocol: LocalInferenceProtocol::OpenaiCompatible,
        base_url: base_url.to_owned(),
        model: provider_model.to_owned(),
        binding_revision: 1,
        context_window,
        max_output_tokens,
        credential: credential.clone(),
        probe: LocalModelProbeState::default(),
    };
    let mut config = astra_credentials::LocalModelConfig::default();
    config.models.insert(name.to_owned(), definition.clone());
    config.validate().map(|()| definition)
}

pub(crate) struct LocalModelCandidate {
    scope: LocalModelScope,
    name: String,
    definition: LocalModelDefinition,
    expected_revision: u64,
    created_secret: Option<String>,
}

impl LocalModelCandidate {
    pub(crate) async fn check(&mut self) -> Result<String, String> {
        let result = check_definition(&self.scope, &self.name, &self.definition)
            .await
            .map_err(|error| error.to_string())?;
        let Some(credential_fingerprint) = result.credential_fingerprint else {
            return Err(
                "provider check completed, but protected local probe identity is unavailable; the candidate was not applied"
                    .to_string(),
            );
        };
        // Keep the probe result on the candidate until the atomic apply. This
        // makes TUI "Test and use" honest: a successful test is visible as
        // ready after saving, while a canceled/failed candidate never mutates
        // the existing binding or its evidence.
        self.definition.probe = LocalModelProbeState::Passed {
            checked_at_unix_ms: now_unix_ms(),
            credential_fingerprint: Some(credential_fingerprint),
        };
        Ok(result.body)
    }

    pub(crate) fn apply(self) -> Result<String, String> {
        save_definition_at(
            &self.scope.models(),
            &self.scope.secrets(),
            LocalModelDefinitionInput {
                name: self.name.clone(),
                definition: self.definition.clone(),
            },
            self.created_secret.clone(),
            Some(self.expected_revision),
        )
    }
}

impl Drop for LocalModelCandidate {
    fn drop(&mut self) {
        if let Some(secret_id) = &self.created_secret {
            // Cancellation and failed probes discard only an unreferenced
            // candidate. An ambiguous durable apply must retain its material.
            if self.scope.models().load().is_ok_and(|config| !config.models.values().any(|model|
                matches!(&model.credential, LocalCredentialRef::ProtectedFile { secret_id: referenced } if referenced == secret_id))) {
                let _ = self.scope.secrets().remove(secret_id);
            }
        }
    }
}

pub(crate) fn prepare_from_tui(
    scope: &LocalModelScope,
    name: String,
    base_url: String,
    provider_model: String,
    context_window: u32,
    max_output_tokens: u32,
    credential_input: LocalModelCredentialInput,
) -> Result<LocalModelCandidate, String> {
    let store = scope.models();
    let expected_revision = store.load().map_err(|error| error.to_string())?.revision;
    let secrets = scope.secrets();
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
    let definition = match validated_local_model_definition(
        &name,
        &base_url,
        &provider_model,
        context_window,
        max_output_tokens,
        &credential,
    ) {
        Ok(definition) => definition,
        Err(error) => {
            // The protected secret is created before the pure validation so
            // the candidate can own it during its lifetime. If validation
            // fails before that candidate exists, clean it up explicitly.
            if let Some(secret_id) = created_secret.as_deref() {
                let _ = secrets.remove(secret_id);
            }
            return Err(error.to_string());
        }
    };
    let candidate = LocalModelCandidate {
        scope: scope.clone(),
        expected_revision,
        name,
        definition,
        created_secret,
    };
    Ok(candidate)
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
    save_definition_at(store, secrets, input, created_secret, None)
}

fn save_definition_at(
    store: &LocalModelConfigStore,
    secrets: &LocalSecretStore,
    input: LocalModelDefinitionInput,
    created_secret: Option<String>,
    expected_revision: Option<u64>,
) -> Result<String, String> {
    let LocalModelDefinitionInput { name, definition } = input;
    let mut publication_attempted = false;
    let apply = (|| {
        let mut config = store.load().map_err(|error| error.to_string())?;
        if expected_revision.is_some_and(|revision| revision != config.revision) {
            return Err("Local configuration changed during setup. Reopen the model and try again; your candidate was not applied.".into());
        }
        let mut definition = definition;
        definition.binding_revision = config.models.get(&name).map_or(Ok(1), |previous| {
            previous.binding_revision.checked_add(1).ok_or_else(|| {
                "local model binding revision is exhausted; remove and recreate the model"
                    .to_string()
            })
        })?;
        // Callers pass `NotRun` for a normal save. A TUI "Test and use"
        // candidate may carry a freshly verified probe that must survive the
        // atomic apply; never erase that evidence here.
        let previous = config.models.insert(name.clone(), definition);
        // Any applied provider material change invalidates prior check
        // evidence. `LocalModelConfigStore` keeps the binding revision
        // monotonic while probe metadata remains separate from that identity.
        let expected_revision = config.revision;
        publication_attempted = true;
        store
            .replace(expected_revision, config)
            .map(|applied| (previous, applied))
            .map_err(|error| error.to_string())
    })();
    let (previous, applied) = match apply {
        Ok(applied) => applied,
        Err(error) => {
            if let Some(secret_id) = created_secret.as_deref() {
                // A rename may have committed before its final sync failed.
                // Never delete a secret that the visible configuration might
                // now reference. Early failures have not attempted publication.
                let unreferenced = !publication_attempted || store.load().is_ok_and(|config| {
                    !config.models.values().any(|model| matches!(&model.credential,
                        LocalCredentialRef::ProtectedFile { secret_id: referenced } if referenced == secret_id))
                });
                if unreferenced {
                    let _ = secrets.remove(secret_id);
                }
            }
            return Err(error);
        }
    };

    if let Some(LocalCredentialRef::ProtectedFile { secret_id }) =
        previous.as_ref().map(|definition| &definition.credential)
    {
        if Some(secret_id.as_str()) != created_secret.as_deref() {
            let _ = secrets.remove(secret_id);
        }
    }
    let (provider_probe, probe_checked_at_unix_ms) = applied
        .models
        .get(&name)
        .map(|definition| {
            (
                probe_status(secrets, definition),
                probe_checked_at(&definition.probe),
            )
        })
        .unwrap_or(("not_run", None));
    let next = format!("astra model local check {name}");
    serde_json::to_string_pretty(&serde_json::json!({
        "name": name,
        "status": "saved_locally",
        "revision": applied.revision,
        "binding_revision": applied
            .models
            .get(&name)
            .map(|definition| definition.binding_revision)
            .unwrap_or_default(),
        "provider_probe": provider_probe,
        "probe_checked_at_unix_ms": probe_checked_at_unix_ms,
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

pub(crate) async fn check(scope: &LocalModelScope, args: ModelCheckArgs) -> Result<String, String> {
    let store = scope.models();
    let config = store.load().map_err(|error| error.to_string())?;
    let definition = config
        .models
        .get(&args.name)
        .ok_or_else(|| format!("local model '{}' is not configured", args.name))?
        .clone();
    let binding_revision = definition.binding_revision;
    match check_definition(scope, &args.name, &definition).await {
        Ok(result) => {
            let persisted = persist_probe_state(
                scope,
                &args.name,
                binding_revision,
                LocalModelProbeState::Passed {
                    checked_at_unix_ms: now_unix_ms(),
                    credential_fingerprint: result.credential_fingerprint,
                },
            );
            let (persisted, warning) = match persisted {
                Ok(ProbePersistOutcome::Persisted) => (true, None),
                Ok(ProbePersistOutcome::CredentialChanged) => (
                    false,
                    Some(
                        "the provider check result was not persisted because another credential observation is retained"
                            .to_string(),
                    ),
                ),
                Ok(ProbePersistOutcome::CredentialFingerprintUnavailable) => (
                    false,
                    Some(
                        "the provider check result was not persisted because protected local probe identity is unavailable"
                            .to_string(),
                    ),
                ),
                Ok(ProbePersistOutcome::ProbeIdentityChanged) => (
                    false,
                    Some(
                        "the provider check result was not persisted because the local credential or probe key changed while the request was running"
                            .to_string(),
                    ),
                ),
                Ok(ProbePersistOutcome::BindingChanged) => (
                    false,
                    Some(
                        "the local model binding changed while the check was running; this result was discarded"
                            .to_string(),
                    ),
                ),
                Ok(ProbePersistOutcome::ModelMissing) => (
                    false,
                    Some(
                        "the local model was removed while the check was running; this result was discarded"
                            .to_string(),
                    ),
                ),
                Err(error) => (false, Some(error)),
            };
            if let Some(warning) = warning {
                // A provider response for an older binding is not evidence
                // about the current model. Keep the result fail-closed so a
                // direct CLI caller cannot mistake `probe_persisted: false`
                // for a successful readiness check.
                return Err(format!(
                    "provider probe completed, but the current local model configuration was not checked: {warning}. Re-run `astra model local check {}`",
                    args.name
                ));
            }
            Ok(annotate_probe_result(&result.body, persisted, None))
        }
        Err(error) => {
            // A failed probe never changes the provider binding or credential.
            // Recording a secret-safe classification makes the next status
            // view useful without retaining provider URLs or response bodies.
            let persisted = persist_probe_state(
                scope,
                &args.name,
                binding_revision,
                LocalModelProbeState::Failed {
                    checked_at_unix_ms: now_unix_ms(),
                    code: probe_failure_code(&error.message),
                    credential_fingerprint: error.credential_fingerprint.clone(),
                },
            );
            match persisted {
                Ok(ProbePersistOutcome::Persisted) => Err(error.to_string()),
                Ok(ProbePersistOutcome::CredentialChanged) => Err(format!(
                    "{} (failure evidence was not persisted because the saved observation belongs to different credential material; run the check again to refresh this terminal's status)",
                    error
                )),
                Ok(ProbePersistOutcome::CredentialFingerprintUnavailable) => Err(format!(
                    "{} (failure evidence was not persisted because protected local probe identity is unavailable)",
                    error
                )),
                Ok(ProbePersistOutcome::ProbeIdentityChanged) => Err(format!(
                    "{} (failure evidence was not persisted because the local credential or probe key changed while the request was running; retry the check for the current identity)",
                    error
                )),
                Ok(ProbePersistOutcome::BindingChanged) => Err(format!(
                    "provider probe failed for an older local model configuration; the binding changed while the check was running, so the result was discarded ({}). Re-run `astra model local check {}` for the current binding",
                    error, args.name
                )),
                Ok(ProbePersistOutcome::ModelMissing) => Err(format!(
                    "provider probe failed for an older local model configuration; model `{}` was removed while the check was running, so the result was discarded ({}). Configure it again before retrying",
                    args.name, error
                )),
                Err(persist_error) => Err(format!(
                    "{} (failure evidence could not be saved: {persist_error}); retry the check for the current status",
                    error
                )),
            }
        }
    }
}

async fn check_definition(
    scope: &LocalModelScope,
    name: &str,
    definition: &LocalModelDefinition,
) -> Result<LocalProbeResult, LocalProbeError> {
    definition
        .validate()
        .map_err(|error| LocalProbeError::new(error.to_string(), None))?;
    let credential = match &definition.credential {
        LocalCredentialRef::Environment { .. } | LocalCredentialRef::None => {
            ResolvedLocalCredential::from_environment(&definition.credential, |name| {
                std::env::var(name).ok()
            })
        }
        LocalCredentialRef::ProtectedFile { .. } | LocalCredentialRef::SystemKeychain { .. } => {
            scope.secrets().resolve(&definition.credential)
        }
    }
    .map_err(|error| LocalProbeError::new(error.to_string(), None))?;
    let probe_key = credential.as_ref().and_then(|_| {
        scope
            .secrets()
            .ensure_probe_fingerprint_key()
            .ok()
            .flatten()
    });
    let credential_fingerprint = match credential.as_ref() {
        Some(credential) => probe_key
            .as_ref()
            .map(|key| credential.fingerprint_with_key(key)),
        None => Some(ResolvedLocalCredential::no_credential_fingerprint()),
    };
    let api_key = credential
        .as_ref()
        .map(ResolvedLocalCredential::expose_to_local_transport)
        .unwrap_or("");
    let mut body = serde_json::json!({
        "model": definition.model,
        "messages": [{"role": "user", "content": "Reply with OK."}],
        "stream": true,
    });
    astra_core::model_wire::apply_chat_output_token_limit(
        &mut body,
        "openai-compatible",
        definition.max_output_tokens.min(4) as usize,
    );
    let request = astra_inference_adapter::ExactProviderRequest::compile(
        &body,
        astra_inference_adapter::ProviderProtocol::OpenAiCompatible,
        64 * 1024,
    )
    .map_err(|error| LocalProbeError::new(error.to_string(), credential_fingerprint.clone()))?;
    let endpoint = chat_completions_endpoint(&definition.base_url)
        .map_err(|error| LocalProbeError::new(error.to_string(), credential_fingerprint.clone()))?;
    let transport = astra_inference_adapter::transport::ProviderTransport::build(
        astra_core::net::runner_provider_client_builder().map_err(|error| {
            LocalProbeError::new(error.to_string(), credential_fingerprint.clone())
        })?,
    )
    .map_err(|error| LocalProbeError::new(error.to_string(), credential_fingerprint.clone()))?;
    let headers = astra_inference_adapter::transport::provider_headers(
        astra_inference_adapter::ProviderProtocol::OpenAiCompatible,
        api_key,
        std::iter::empty::<(&str, &str)>(),
    )
    .map_err(|error| LocalProbeError::new(error.to_string(), credential_fingerprint.clone()))?;
    let attempt = transport
        .prepare(
            &endpoint,
            headers,
            &request,
            Some(std::time::Duration::from_secs(20)),
        )
        .map_err(|error| LocalProbeError::new(error.to_string(), credential_fingerprint.clone()))?;
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
        let mut saw_semantic_event = false;
        let mut saw_finish_reason = false;
        let mut saw_provider_error = false;
        let mut done = false;
        while let Some(event) = events_rx.recv().await {
            match event {
                astra_inference_adapter::transport::ProviderEvent::Json(value) => {
                    json_events += 1;
                    let payload = astra_inference_adapter::openai::OpenAiPayload::stream(&value);
                    // Keep the local readiness probe aligned with the same
                    // OpenAI projection used by the canonical Runner
                    // collector. A non-empty `choices` array alone accepts
                    // malformed chunks such as `choices:[{}]` and an EOF
                    // after content without a terminal marker.
                    saw_semantic_event |= payload.message_present;
                    saw_finish_reason |= payload.finish_reason.is_some();
                    saw_provider_error |= value.get("error").is_some();
                }
                astra_inference_adapter::transport::ProviderEvent::Done => done = true,
                astra_inference_adapter::transport::ProviderEvent::Eof => {}
            }
        }
        (
            json_events,
            saw_semantic_event,
            saw_finish_reason,
            saw_provider_error,
            done,
        )
    };
    let (terminal, (json_events, saw_semantic_event, saw_finish_reason, saw_provider_error, done)) =
        tokio::join!(execute, drain);
    if terminal.status != astra_inference_adapter::transport::ExecutionStatus::Complete
        || json_events == 0
        || !saw_semantic_event
        || (!done && !saw_finish_reason)
        || saw_provider_error
    {
        return Err(LocalProbeError::new(
            format!(
                "provider probe failed ({:?}); no retry was attempted",
                terminal.status
            ),
            credential_fingerprint.clone(),
        ));
    }
    let body = serde_json::to_string_pretty(&LocalModelStatus {
        name,
        source: credential_kind(&definition.credential),
        configuration: "valid",
        credential: "available",
        provider_probe: if done {
            "stream_verified"
        } else {
            "stream_eof_verified"
        },
        config_path: scope.models().path().display().to_string(),
    })
    .map_err(|error| LocalProbeError::new(error.to_string(), credential_fingerprint.clone()))?;
    drop(credential);
    Ok(LocalProbeResult {
        body,
        credential_fingerprint,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbePersistOutcome {
    Persisted,
    CredentialChanged,
    CredentialFingerprintUnavailable,
    ProbeIdentityChanged,
    BindingChanged,
    ModelMissing,
}

fn persist_probe_state(
    scope: &LocalModelScope,
    name: &str,
    binding_revision: u64,
    probe: LocalModelProbeState,
) -> Result<ProbePersistOutcome, String> {
    let store = scope.models();
    // An unrelated model update may win the file CAS between load and
    // replace. Retry once while the binding revision is still the one that
    // was checked; this preserves evidence without ever attaching it to a
    // replacement provider binding.
    for attempt in 0..2 {
        let mut config = store.load().map_err(|error| error.to_string())?;
        let Some(definition) = config.models.get_mut(name) else {
            return Ok(ProbePersistOutcome::ModelMissing);
        };
        // A check may finish after another terminal replaced the binding. Do
        // not attach stale evidence to the newer provider configuration.
        if definition.binding_revision != binding_revision {
            return Ok(ProbePersistOutcome::BindingChanged);
        }
        if probe_credential_fingerprint(&probe).is_none()
            && !matches!(definition.credential, LocalCredentialRef::None)
        {
            // Never persist an environment/protected-credential observation
            // without a keyed material identity. Such a record would look
            // like a shared success to another terminal.
            return Ok(ProbePersistOutcome::CredentialFingerprintUnavailable);
        }
        if !matches!(&definition.credential, LocalCredentialRef::None) {
            // The credential and owner-local probe key can change while a
            // provider request is in flight. Recompute the material-bound
            // identity before the CAS so a delayed result from an older
            // generation cannot overwrite a newer terminal's evidence.
            let Some(incoming) = probe_credential_fingerprint(&probe) else {
                return Ok(ProbePersistOutcome::CredentialFingerprintUnavailable);
            };
            let Some(current) =
                current_credential_fingerprint(&scope.secrets(), &definition.credential)
            else {
                return Ok(ProbePersistOutcome::ProbeIdentityChanged);
            };
            if !fingerprints_equal(&current, incoming) {
                return Ok(ProbePersistOutcome::ProbeIdentityChanged);
            }
        }
        // A failed check from another terminal must not erase a known-good
        // observation for the credential material that this terminal uses.
        // Keep the existing observation as historical evidence; the caller
        // still receives the provider failure and can explicitly retry.
        if let LocalModelProbeState::Failed {
            credential_fingerprint: incoming,
            ..
        } = &probe
        {
            let existing = probe_credential_fingerprint(&definition.probe);
            let same_key_generation = existing
                .zip(incoming.as_deref())
                .and_then(|(existing, incoming)| {
                    Some((
                        ResolvedLocalCredential::fingerprint_key_generation(existing)?,
                        ResolvedLocalCredential::fingerprint_key_generation(incoming)?,
                    ))
                })
                .is_some_and(|(existing, incoming)| existing == incoming);
            if existing.is_some() && existing != incoming.as_deref() && same_key_generation {
                return Ok(ProbePersistOutcome::CredentialChanged);
            }
        }
        definition.probe = probe.clone();
        let expected_revision = config.revision;
        match store.replace(expected_revision, config) {
            Ok(_) => return Ok(ProbePersistOutcome::Persisted),
            Err(LocalModelConfigError::RevisionConflict { .. }) if attempt == 0 => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("probe persistence retry exhausted".to_string())
}

fn annotate_probe_result(body: &str, persisted: bool, warning: Option<&str>) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("probe_persisted".into(), serde_json::Value::Bool(persisted));
        if let Some(warning) = warning {
            object.insert(
                "probe_persistence_warning".into(),
                serde_json::Value::String(warning.to_string()),
            );
        }
    }
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_string())
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn probe_failure_code(error: &str) -> String {
    let code = if error.contains("HttpStatus(401)") {
        "http_401"
    } else if error.contains("HttpStatus(403)") {
        "http_403"
    } else if error.contains("HttpStatus(404)") {
        "http_404"
    } else if error.contains("Deadline") || error.contains("timed out") {
        "timeout"
    } else if error.contains("Transport") || error.contains("connection") {
        "transport"
    } else if error.contains("Protocol") {
        "protocol"
    } else {
        "probe_failed"
    };
    code.to_string()
}

pub(crate) fn show(scope: &LocalModelScope, name: &str) -> Result<Option<String>, String> {
    let store = scope.models();
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
        "configuration": "valid",
        "credential": credential_availability(scope, &definition.credential),
        "provider_probe": probe_status(&scope.secrets(), definition),
        "probe_checked_at_unix_ms": probe_checked_at(&definition.probe),
        "probe_failure_code": probe_failure(&definition.probe),
        "status": local_model_status(scope, definition),
        "next": local_model_next_step(name, scope, definition),
        "revision": config.revision,
        "config_path": store.path(),
    }))
    .map(Some)
    .map_err(|error| error.to_string())
}

/// Return all local model bindings and their actionable, local-only readiness
/// state. This never contacts the provider, starts a Runner, or includes a
/// secret value, so it is safe for a status screen and scripts.
pub(crate) fn list(scope: &LocalModelScope) -> Result<String, String> {
    let store = scope.models();
    let config = store.load().map_err(|error| error.to_string())?;
    let models: Vec<_> = config
        .models
        .iter()
        .map(|(name, definition)| LocalModelStatusRow {
            name: name.clone(),
            model: definition.model.clone(),
            binding_revision: definition.binding_revision,
            credential_source: credential_kind(&definition.credential),
            credential: credential_availability(scope, &definition.credential),
            provider_probe: probe_status(&scope.secrets(), definition),
            probe_checked_at_unix_ms: probe_checked_at(&definition.probe),
            probe_failure_code: probe_failure(&definition.probe),
            status: local_model_status(scope, definition),
            next: local_model_next_step(name, scope, definition),
        })
        .collect();
    let has_models = !models.is_empty();
    serde_json::to_string_pretty(&serde_json::json!({
        "scope": "runner_local",
        "models": models,
        "config_revision": config.revision,
        "config_path": store.path(),
        "next": if !has_models {
            "astra model local add"
        } else {
            "astra model local check <name>"
        },
    }))
    .map_err(|error| error.to_string())
}

pub(crate) fn remove(scope: &LocalModelScope, args: ModelRemoveArgs) -> Result<String, String> {
    let store = scope.models();
    let secrets = scope.secrets();
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

fn credential_availability(
    scope: &LocalModelScope,
    reference: &LocalCredentialRef,
) -> &'static str {
    match reference {
        LocalCredentialRef::Environment { name } => match std::env::var(name) {
            Ok(value) if !value.is_empty() => "available",
            _ => "missing",
        },
        LocalCredentialRef::ProtectedFile { .. } => match scope.secrets().resolve(reference) {
            Ok(Some(_)) => "available",
            _ => "missing",
        },
        LocalCredentialRef::SystemKeychain { .. } => "unsupported",
        LocalCredentialRef::None => "not_required",
    }
}

fn current_credential_fingerprint(
    secrets: &LocalSecretStore,
    reference: &LocalCredentialRef,
) -> Option<String> {
    match reference {
        LocalCredentialRef::Environment { .. } => {
            let key = secrets.resolve_probe_fingerprint_key().ok().flatten()?;
            ResolvedLocalCredential::from_environment(reference, |name| std::env::var(name).ok())
                .ok()
                .flatten()
                .map(|credential| credential.fingerprint_with_key(&key))
        }
        LocalCredentialRef::ProtectedFile { .. } => secrets
            .resolve_probe_fingerprint_key()
            .ok()
            .flatten()
            .zip(secrets.resolve(reference).ok().flatten())
            .map(|(key, credential)| credential.fingerprint_with_key(&key)),
        LocalCredentialRef::None => Some(ResolvedLocalCredential::no_credential_fingerprint()),
        LocalCredentialRef::SystemKeychain { .. } => None,
    }
}

fn probe_credential_fingerprint(probe: &LocalModelProbeState) -> Option<&str> {
    match probe {
        LocalModelProbeState::Passed {
            credential_fingerprint,
            ..
        }
        | LocalModelProbeState::Failed {
            credential_fingerprint,
            ..
        } => credential_fingerprint.as_deref(),
        LocalModelProbeState::NotRun => None,
    }
}

fn probe_matches_current_credential(
    secrets: &LocalSecretStore,
    definition: &LocalModelDefinition,
) -> bool {
    let Some(stored) = probe_credential_fingerprint(&definition.probe) else {
        // A legacy no-auth record has no terminal-local material to bind. It
        // remains safe to read as historical evidence; legacy environment
        // records are deliberately stale until checked again.
        return matches!(definition.credential, LocalCredentialRef::None);
    };
    current_credential_fingerprint(secrets, &definition.credential)
        .is_some_and(|current| fingerprints_equal(&current, stored))
}

fn fingerprints_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn probe_status(secrets: &LocalSecretStore, definition: &LocalModelDefinition) -> &'static str {
    match &definition.probe {
        LocalModelProbeState::NotRun => "not_run",
        LocalModelProbeState::Passed { .. }
            if probe_matches_current_credential(secrets, definition) =>
        {
            "stream_verified"
        }
        LocalModelProbeState::Failed { .. }
            if probe_matches_current_credential(secrets, definition) =>
        {
            "failed"
        }
        LocalModelProbeState::Passed { .. } | LocalModelProbeState::Failed { .. } => "stale",
    }
}

fn probe_checked_at(probe: &LocalModelProbeState) -> Option<u64> {
    match probe {
        LocalModelProbeState::NotRun => None,
        LocalModelProbeState::Passed {
            checked_at_unix_ms, ..
        }
        | LocalModelProbeState::Failed {
            checked_at_unix_ms, ..
        } => Some(*checked_at_unix_ms),
    }
}

fn probe_failure(probe: &LocalModelProbeState) -> Option<String> {
    match probe {
        LocalModelProbeState::Failed { code, .. } => Some(code.clone()),
        _ => None,
    }
}

fn local_model_status(scope: &LocalModelScope, definition: &LocalModelDefinition) -> &'static str {
    if !matches!(
        credential_availability(scope, &definition.credential),
        "available" | "not_required"
    ) {
        return "needs_attention";
    }
    match &definition.probe {
        LocalModelProbeState::NotRun => "ready_for_check",
        LocalModelProbeState::Passed { .. }
            if probe_matches_current_credential(&scope.secrets(), definition) =>
        {
            "ready"
        }
        LocalModelProbeState::Failed { .. }
            if probe_matches_current_credential(&scope.secrets(), definition) =>
        {
            "needs_attention"
        }
        LocalModelProbeState::Passed { .. } | LocalModelProbeState::Failed { .. } => {
            "ready_for_check"
        }
    }
}

fn local_model_next_step(
    name: &str,
    scope: &LocalModelScope,
    definition: &LocalModelDefinition,
) -> String {
    match credential_availability(scope, &definition.credential) {
        "missing" => {
            format!("set the configured credential, then run astra model local check {name}")
        }
        "unsupported" => {
            "use --credential-env or a supported protected credential backend".to_string()
        }
        _ => match &definition.probe {
            LocalModelProbeState::NotRun => {
                format!("run astra model local check {name} to verify the provider")
            }
            LocalModelProbeState::Passed { .. }
                if probe_matches_current_credential(&scope.secrets(), definition) =>
            {
                "select it with /model".to_string()
            }
            LocalModelProbeState::Failed { .. }
                if probe_matches_current_credential(&scope.secrets(), definition) =>
            {
                format!("fix the reported issue, then run astra model local check {name}")
            }
            LocalModelProbeState::Passed { .. } | LocalModelProbeState::Failed { .. } => {
                format!(
                    "credential material changed since the last provider check; run astra model local check {name}"
                )
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn scope() -> LocalModelScope {
        LocalModelScope::for_owner("https://astra.example", "fixture-owner").unwrap()
    }
    fn add(args: ModelAddArgs) -> Result<String, String> {
        super::add(&scope(), args)
    }
    async fn check(args: ModelCheckArgs) -> Result<String, String> {
        super::check(&scope(), args).await
    }
    fn show(name: &str) -> Result<Option<String>, String> {
        super::show(&scope(), name)
    }
    fn remove(args: ModelRemoveArgs) -> Result<String, String> {
        super::remove(&scope(), args)
    }

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

    #[tokio::test]
    #[serial]
    async fn failed_candidate_probe_preserves_old_configuration_and_secret() {
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let scope = scope();
        prepare_from_tui(
            &scope,
            "work".into(),
            "http://127.0.0.1:9".into(),
            "original".into(),
            1024,
            64,
            LocalModelCredentialInput::Stored("original-key".into()),
        )
        .unwrap()
        .apply()
        .unwrap();
        let original = scope.models().load().unwrap();
        let mut candidate = prepare_from_tui(
            &scope,
            "work".into(),
            "http://127.0.0.1:9".into(),
            "candidate".into(),
            1024,
            64,
            LocalModelCredentialInput::Stored("candidate-key".into()),
        )
        .unwrap();
        let candidate_ref = candidate.definition.credential.clone();
        assert!(candidate.check().await.is_err());
        drop(candidate);
        assert!(scope.models().load().unwrap() == original);
        assert!(
            scope
                .secrets()
                .resolve(&original.models["work"].credential)
                .unwrap()
                .is_some()
        );
        assert!(scope.secrets().resolve(&candidate_ref).is_err());
    }

    #[test]
    #[serial]
    fn candidate_apply_rejects_concurrent_configuration_change() {
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let scope = scope();
        let candidate = prepare_from_tui(
            &scope,
            "work".into(),
            "http://127.0.0.1:9".into(),
            "candidate".into(),
            1024,
            64,
            LocalModelCredentialInput::None,
        )
        .unwrap();
        add(no_auth_add("other")).unwrap();
        assert!(
            candidate
                .apply()
                .unwrap_err()
                .contains("changed during setup")
        );
        let current = scope.models().load().unwrap();
        assert!(current.models.contains_key("other"));
        assert!(!current.models.contains_key("work"));
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
        assert_eq!(other["binding_revision"], 2);

        add(no_auth_add("work")).unwrap();
        let updated: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert_eq!(updated["binding_revision"], 3);
        let other_after: serde_json::Value =
            serde_json::from_str(&show("other").unwrap().unwrap()).unwrap();
        assert_eq!(other_after["binding_revision"], other["binding_revision"]);

        let removed = remove(ModelRemoveArgs {
            name: "work".to_string(),
        })
        .unwrap();
        assert!(removed.contains("removed_locally"));
        assert!(show("work").unwrap().is_none());
        add(no_auth_add("work")).unwrap();
        let recreated: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert!(
            recreated["binding_revision"].as_u64().unwrap()
                > updated["binding_revision"].as_u64().unwrap()
        );
    }

    #[test]
    #[serial]
    fn local_model_list_reports_safe_state_and_next_action_without_provider_io() {
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        add(no_auth_add("ready")).unwrap();
        add(ModelAddArgs {
            name: Some("missing-key".into()),
            base_url: Some("http://127.0.0.1:8080/v1".into()),
            provider_model: Some("coding-model".into()),
            context_window: Some(128_000),
            max_output_tokens: Some(8_192),
            credential_env: Some("ASTRA_MISSING_PROVIDER_KEY".into()),
            no_auth: false,
            store_secret: false,
        })
        .unwrap();

        let listed: serde_json::Value = serde_json::from_str(&list(&scope()).unwrap()).unwrap();
        assert_eq!(listed["models"].as_array().unwrap().len(), 2);
        let ready = listed["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["name"] == "ready")
            .unwrap();
        assert_eq!(ready["status"], "ready_for_check");
        assert_eq!(ready["credential"], "not_required");
        assert!(ready["next"].as_str().unwrap().contains("check ready"));
        let missing = listed["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["name"] == "missing-key")
            .unwrap();
        assert_eq!(missing["status"], "needs_attention");
        assert_eq!(missing["credential"], "missing");
        assert!(
            missing["next"]
                .as_str()
                .unwrap()
                .contains("set the configured credential")
        );
        assert!(
            std::fs::read_dir(scope().root().join("model-secrets"))
                .map(|entries| entries.count() == 0)
                .unwrap_or(true),
            "status must not create or copy credentials"
        );
    }

    #[test]
    #[serial]
    fn stale_probe_evidence_cannot_mark_a_newer_binding_ready() {
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        add(no_auth_add("work")).unwrap();
        let old_binding = scope().models().load().unwrap().models["work"].binding_revision;
        add(ModelAddArgs {
            provider_model: Some("new-model".into()),
            ..no_auth_add("work")
        })
        .unwrap();
        assert_eq!(
            persist_probe_state(
                &scope(),
                "work",
                old_binding,
                LocalModelProbeState::Passed {
                    checked_at_unix_ms: 1,
                    credential_fingerprint: Some(
                        ResolvedLocalCredential::no_credential_fingerprint(),
                    ),
                },
            )
            .unwrap(),
            ProbePersistOutcome::BindingChanged
        );
        let current = scope().models().load().unwrap();
        assert_eq!(current.models["work"].model, "new-model");
        assert!(matches!(
            current.models["work"].probe,
            LocalModelProbeState::NotRun
        ));
    }

    #[tokio::test]
    #[serial]
    async fn delayed_probe_cannot_publish_success_for_a_replaced_binding() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(100))
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let scope = scope();
        add(ModelAddArgs {
            base_url: Some(format!("{}/v1", server.uri())),
            ..no_auth_add("work")
        })
        .unwrap();

        let check_scope = scope.clone();
        let checking = tokio::spawn(async move {
            super::check(
                &check_scope,
                ModelCheckArgs {
                    name: "work".to_string(),
                },
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if server.received_requests().await.unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delayed provider request must start");

        add(ModelAddArgs {
            provider_model: Some("replacement-model".into()),
            ..no_auth_add("work")
        })
        .unwrap();
        let result = checking.await.unwrap();
        let error = result.expect_err("a stale success must be a failed check");
        assert!(
            error.contains("current local model configuration was not checked"),
            "{error}"
        );
        let current = scope.models().load().unwrap();
        assert_eq!(current.models["work"].model, "replacement-model");
        assert!(matches!(
            current.models["work"].probe,
            LocalModelProbeState::NotRun
        ));
    }

    #[tokio::test]
    #[serial]
    async fn delayed_probe_cannot_publish_success_after_binding_removal() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(100))
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let scope = scope();
        add(ModelAddArgs {
            base_url: Some(format!("{}/v1", server.uri())),
            ..no_auth_add("work")
        })
        .unwrap();

        let check_scope = scope.clone();
        let checking = tokio::spawn(async move {
            super::check(
                &check_scope,
                ModelCheckArgs {
                    name: "work".to_string(),
                },
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if server.received_requests().await.unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delayed provider request must start");

        remove(ModelRemoveArgs {
            name: "work".to_string(),
        })
        .unwrap();
        let result = checking.await.unwrap();
        let error = result.expect_err("a removed binding must not receive readiness evidence");
        assert!(
            error.contains("current local model configuration was not checked"),
            "{error}"
        );
        assert!(scope.models().load().unwrap().models.is_empty());
    }

    #[test]
    #[cfg(unix)]
    #[serial]
    fn failed_local_model_update_removes_new_secret_on_early_failure() {
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        add(no_auth_add("work")).unwrap();
        let scope = scope();
        let mut config = scope.models().load().unwrap();
        config.models.get_mut("work").unwrap().binding_revision = u64::MAX;
        scope.models().replace(config.revision, config).unwrap();
        for malformed in [false, true] {
            if malformed {
                std::fs::write(scope.models().path(), "malformed-test-config").unwrap();
            }
            let before = std::fs::read(scope.models().path()).unwrap();
            assert!(
                prepare_from_tui(
                    &scope,
                    "work".into(),
                    "https://provider.example/v1".into(),
                    "coding-model".into(),
                    128000,
                    8192,
                    LocalModelCredentialInput::Stored("canary-key".into())
                )
                .and_then(|candidate| candidate.apply())
                .is_err()
            );
            assert_eq!(std::fs::read(scope.models().path()).unwrap(), before);
            assert_eq!(
                std::fs::read_dir(scope.root().join("model-secrets"))
                    .unwrap()
                    .count(),
                0,
                "failed validation/load must not orphan newly created secrets"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    #[serial]
    fn invalid_tui_candidate_removes_secret_created_before_validation() {
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let scope = scope();
        let error = match prepare_from_tui(
            &scope,
            "work".into(),
            "not-a-provider-url".into(),
            "coding-model".into(),
            128_000,
            8_192,
            LocalModelCredentialInput::Stored("canary-key".into()),
        ) {
            Ok(_) => panic!("invalid candidate must not be prepared"),
            Err(error) => error,
        };
        assert!(error.contains("base URL"), "{error}");
        assert_eq!(
            std::fs::read_dir(scope.root().join("model-secrets"))
                .unwrap()
                .count(),
            0,
            "validation failure must not orphan a newly created secret"
        );
    }

    #[test]
    fn provider_endpoint_is_derived_without_rewriting_query_or_duplicating_path() {
        for (input, expected) in [
            ("/v1/", "/v1/chat/completions"),
            ("/v1/chat/completions/", "/v1/chat/completions/"),
            ("/tenant%2Fone/v1", "/tenant%2Fone/v1/chat/completions"),
        ] {
            assert_eq!(
                chat_completions_endpoint(&format!("https://provider.example{input}?route=a%2Fb"))
                    .unwrap(),
                format!("https://provider.example{expected}?route=a%2Fb")
            );
        }
        assert!(chat_completions_endpoint("not a URL").is_err());
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
    #[cfg(unix)]
    async fn saved_endpoint_probe_and_granted_dispatch_share_strict_wire_contract() {
        use astra_edge::inference_host::{
            DispatchOutcome, GrantClock, InferenceHost, InferenceOwner,
        };
        use astra_inference_adapter::transport::ProviderTransport;
        use astra_inference_adapter::{ExactProviderRequest, ProviderProtocol};
        use astra_turn_types::runner_inference::*;
        use std::num::NonZeroU64;
        use std::time::Duration;
        use tokio::time::Instant;
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        fn id(value: &str) -> RunnerInferenceId {
            RunnerInferenceId::new(value).unwrap()
        }
        // This fixture rejects the old probe dialect and wrong model/key/path.
        // Both explicit check and real host dispatch must pass the same validator.
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(|request: &Request| {
                if request.url.path() != "/gateway/v1/chat/completions"
                    || request.url.query() != Some("api-version=1&route=a%2Fb")
                {
                    return ResponseTemplate::new(404);
                }
                if request.headers.get("authorization").and_then(|v| v.to_str().ok())
                    != Some("Bearer fixture-key")
                {
                    return ResponseTemplate::new(401);
                }
                let body: serde_json::Value = request.body_json().unwrap();
                if body["model"] != "o3"
                    || body.get("max_tokens").is_some()
                    || !body["max_completion_tokens"].as_u64().is_some_and(|n| (1..=4).contains(&n))
                    || body["stream"] != true
                {
                    return ResponseTemplate::new(400);
                }
                ResponseTemplate::new(200).set_body_raw(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                )
            })
            .mount(&server).await;
        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let scope = scope();
        for (index, suffix) in ["/gateway/v1", "/gateway/v1/chat/completions"]
            .iter()
            .enumerate()
        {
            let endpoint = format!("{}{suffix}?api-version=1&route=a%2Fb", server.uri());
            prepare_from_tui(
                &scope,
                "work".into(),
                endpoint,
                "o3".into(),
                1024,
                4,
                LocalModelCredentialInput::Stored("fixture-key".into()),
            )
            .and_then(|candidate| candidate.apply())
            .unwrap();
            assert!(
                super::check(
                    &scope,
                    ModelCheckArgs {
                        name: "work".into()
                    }
                )
                .await
                .unwrap()
                .contains("stream_verified")
            );
            let host = InferenceHost::open(
                root.path().join(format!("journal-{index}")),
                InferenceOwner {
                    deployment_identity: "fixture".into(),
                    user_id: "fixture-user".into(),
                    runner_id: id("fixture-runner"),
                },
                scope.models().path().to_path_buf(),
                scope.root().join("model-secrets"),
                ProviderTransport::build(reqwest::Client::builder().no_proxy()).unwrap(),
            )
            .await
            .unwrap();
            let mut body = serde_json::json!({"model":"o3", "messages":[{"role":"user","content":"Hello"}], "stream":true});
            astra_core::model_wire::apply_chat_output_token_limit(
                &mut body,
                "openai-compatible",
                4,
            );
            let artifact =
                ExactProviderRequest::compile(&body, ProviderProtocol::OpenAiCompatible, 65536)
                    .unwrap();
            let body = String::from_utf8(artifact.body().to_vec()).unwrap();
            let grant = RunnerInferenceDispatchGrant {
                attempt: RunnerInferenceAttemptIdentity {
                    user_id: "fixture-user".into(),
                    scope: astra_turn_types::InferenceInvocationScope::Session {
                        session_id: "fixture-session".into(),
                        turn: 0,
                        round: 0,
                        operation_id: "fixture-operation".into(),
                        logical_attempt: 0,
                    },
                    invocation_id: id("fixture-invocation"),
                    attempt_id: id("fixture-attempt"),
                    binding: host.bindings().await.unwrap().remove(0).identity,
                    request: RunnerInferenceArtifactReference {
                        artifact_id: id("fixture-artifact"),
                        sha256: RunnerInferenceDigest::new(artifact.identity().sha256.clone())
                            .unwrap(),
                        byte_len: NonZeroU64::new(body.len() as u64).unwrap(),
                    },
                },
                grant_id: id("fixture-grant"),
                process_boot_nonce: host.process_boot_nonce().clone(),
                start_before_unix_ms: 1_060_000,
                deadline_unix_ms: 1_120_000,
            };
            let now = Instant::now();
            assert!(matches!(
                host.dispatch(
                    grant,
                    body,
                    GrantClock::observed(1_000_000, now, now).unwrap()
                )
                .await
                .unwrap(),
                DispatchOutcome::Started
            ));
            let terminal = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some((_, terminal)) = host.pending(1).await.unwrap().pop() {
                        break terminal;
                    }
                    host.terminal_ready().await;
                }
            })
            .await
            .unwrap();
            let response: RunnerInferenceResponse =
                serde_json::from_str(&terminal.response_json).unwrap();
            assert_eq!(
                response.transport.status,
                RunnerInferenceTransportStatus::Complete
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                (index + 1) * 2
            );
        }
        for (model, key, status) in [
            ("o3", "invalid-key", "HttpStatus(401)"),
            ("invalid-model", "fixture-key", "HttpStatus(400)"),
        ] {
            prepare_from_tui(
                &scope,
                "work".into(),
                format!(
                    "{}/gateway/v1/chat/completions?api-version=1&route=a%2Fb",
                    server.uri()
                ),
                model.into(),
                1024,
                4,
                LocalModelCredentialInput::Stored(key.into()),
            )
            .and_then(|candidate| candidate.apply())
            .unwrap();
            let before = scope.models().load().unwrap();
            let calls = server.received_requests().await.unwrap().len();
            let error = super::check(
                &scope,
                ModelCheckArgs {
                    name: "work".into(),
                },
            )
            .await
            .unwrap_err();
            assert!(error.contains(status), "{error}");
            assert_eq!(server.received_requests().await.unwrap().len(), calls + 1);
            let after = scope.models().load().unwrap();
            let before_definition = before.models["work"].clone();
            let mut after_definition = after.models["work"].clone();
            after_definition.probe = before_definition.probe.clone();
            assert_eq!(after_definition, before_definition);
            assert!(matches!(
                after.models["work"].probe,
                LocalModelProbeState::Failed { .. }
            ));
        }
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
        assert!(checked.contains("\"probe_persisted\": true"));
        let saved: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert_eq!(saved["provider_probe"], "stream_verified");
        assert_eq!(saved["status"], "ready");
        assert!(saved["probe_checked_at_unix_ms"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    #[serial]
    #[cfg(unix)]
    async fn environment_probe_evidence_is_scoped_to_current_terminal_material() {
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer password"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer other-password"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let terminal_a = scope();
        let terminal_b = scope();
        let variable = "ASTRA_TEST_TERMINAL_PROVIDER_KEY";
        // Keep the canary intentionally low entropy. A plain digest stored in
        // models.json would be offline-enumerable; the persisted HMAC must
        // remain unverifiable without the owner-local probe key.
        let mut environment = TestEnvironmentVariable::set(variable, "password");
        add(ModelAddArgs {
            name: Some("work".into()),
            base_url: Some(format!("{}/v1", server.uri())),
            provider_model: Some("coding-model".into()),
            context_window: Some(128_000),
            max_output_tokens: Some(8_192),
            credential_env: Some(variable.into()),
            no_auth: false,
            store_secret: false,
        })
        .unwrap();

        super::check(
            &terminal_a,
            ModelCheckArgs {
                name: "work".into(),
            },
        )
        .await
        .unwrap();
        let listed: serde_json::Value =
            serde_json::from_str(&super::list(&terminal_a).unwrap()).unwrap();
        assert_eq!(listed["models"][0]["provider_probe"], "stream_verified");
        assert_eq!(listed["models"][0]["status"], "ready");

        let models_json = std::fs::read_to_string(terminal_a.models().path()).unwrap();
        assert!(models_json.contains("hmac-sha256-v1:"));
        assert!(!models_json.contains("password"));
        let mut legacy = Sha256::new();
        legacy.update(b"astra-local-model-credential-v1\0");
        legacy.update(b"password");
        assert!(!models_json.contains(&format!("sha256:{:x}", legacy.finalize())));

        let probe_key_path = terminal_a
            .root()
            .join("model-secrets")
            .join("probe_fingerprint_key_v1");
        assert!(probe_key_path.is_file());
        let probe_key = std::fs::read_to_string(&probe_key_path).unwrap();
        assert!(!models_json.contains(&probe_key));

        // A second terminal can use the same owner-scoped config, but its
        // environment value is a different credential attachment. Listing is
        // local-only and must never claim that terminal B was checked.
        environment.replace("other-password");
        let listed: serde_json::Value =
            serde_json::from_str(&super::list(&terminal_b).unwrap()).unwrap();
        assert_eq!(listed["models"][0]["provider_probe"], "stale");
        assert_eq!(listed["models"][0]["status"], "ready_for_check");
        assert!(
            listed["models"][0]["next"]
                .as_str()
                .unwrap()
                .contains("credential material changed")
        );

        let error = super::check(
            &terminal_b,
            ModelCheckArgs {
                name: "work".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("HttpStatus(401)"), "{error}");
        let listed: serde_json::Value =
            serde_json::from_str(&super::list(&terminal_b).unwrap()).unwrap();
        assert_eq!(listed["models"][0]["provider_probe"], "stale");
        assert_eq!(listed["models"][0]["status"], "ready_for_check");
        assert!(
            listed["models"][0]["next"]
                .as_str()
                .unwrap()
                .contains("credential material changed")
        );

        // A failed check from terminal B does not erase terminal A's known-good
        // observation. Rotating back therefore remains ready, while terminal
        // B must explicitly check its own material before it can be selected.
        environment.replace("password");
        let listed: serde_json::Value =
            serde_json::from_str(&super::list(&terminal_a).unwrap()).unwrap();
        assert_eq!(listed["models"][0]["provider_probe"], "stream_verified");
        assert_eq!(listed["models"][0]["status"], "ready");

        // Losing the owner-local key invalidates the old HMAC generation. A
        // failed recheck must replace that now-unverifiable success with the
        // current failure instead of preserving stale evidence forever.
        std::fs::remove_file(&probe_key_path).unwrap();
        environment.replace("other-password");
        let error = super::check(
            &terminal_b,
            ModelCheckArgs {
                name: "work".to_string(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("HttpStatus(401)"), "{error}");
        let listed: serde_json::Value =
            serde_json::from_str(&super::list(&terminal_b).unwrap()).unwrap();
        assert_eq!(listed["models"][0]["provider_probe"], "failed");
        assert_eq!(listed["models"][0]["status"], "needs_attention");
        assert_eq!(listed["models"][0]["probe_failure_code"], "http_401");
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn two_process_probe_worker() {
        let Some(role) = std::env::var("ASTRA_TWO_PROCESS_PROBE_ROLE").ok() else {
            // This test is launched explicitly by the parent process below.
            return;
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(super::check(
            &scope(),
            ModelCheckArgs {
                name: "work".to_string(),
            },
        ));
        match role.as_str() {
            "valid" => assert!(result.is_ok(), "{result:?}"),
            "invalid" => {
                let error = result.expect_err("the invalid child credential must fail");
                assert!(error.contains("HttpStatus(401)"), "{error}");
                let listed: serde_json::Value =
                    serde_json::from_str(&super::list(&scope()).unwrap()).unwrap();
                assert_eq!(listed["models"][0]["provider_probe"], "stale");
                assert_eq!(listed["models"][0]["status"], "ready_for_check");
            }
            other => panic!("unexpected probe worker role {other}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    #[cfg(unix)]
    async fn environment_probe_evidence_isolated_across_processes() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer process-valid"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer process-invalid"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let _credentials_dir = TestEnvironmentVariable::set(
            "ASTRA_CLI_CREDENTIALS_DIR",
            root.path().to_str().unwrap(),
        );
        let scope = scope();
        let variable = "ASTRA_TEST_TERMINAL_PROVIDER_KEY";
        add(ModelAddArgs {
            name: Some("work".into()),
            base_url: Some(format!("{}/v1", server.uri())),
            provider_model: Some("coding-model".into()),
            context_window: Some(128_000),
            max_output_tokens: Some(8_192),
            credential_env: Some(variable.into()),
            no_auth: false,
            store_secret: false,
        })
        .unwrap();

        let executable = std::env::current_exe().unwrap();
        let run_worker = |role: &str, credential: &str| {
            std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "cli::local_model_command::tests::two_process_probe_worker",
                    "--nocapture",
                ])
                .env("ASTRA_CLI_CREDENTIALS_DIR", root.path())
                .env("ASTRA_TWO_PROCESS_PROBE_ROLE", role)
                .env(variable, credential)
                .output()
                .unwrap()
        };
        let valid = run_worker("valid", "process-valid");
        assert!(
            valid.status.success(),
            "valid child failed: {}\n{}",
            valid.status,
            String::from_utf8_lossy(&valid.stderr)
        );
        let stored = scope.models().load().unwrap();
        assert!(matches!(
            stored.models["work"].probe,
            LocalModelProbeState::Passed { .. }
        ));

        let invalid = run_worker("invalid", "process-invalid");
        assert!(
            invalid.status.success(),
            "invalid child failed: {}\n{}",
            invalid.status,
            String::from_utf8_lossy(&invalid.stderr)
        );
        let stored = scope.models().load().unwrap();
        assert!(matches!(
            stored.models["work"].probe,
            LocalModelProbeState::Passed { .. }
        ));
    }

    #[tokio::test]
    #[serial]
    #[cfg(unix)]
    async fn delayed_probe_from_old_key_cannot_overwrite_new_generation() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer password"))
            .respond_with(
                ResponseTemplate::new(401).set_delay(std::time::Duration::from_millis(500)),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer other-password"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;

        let root = tempfile::tempdir().unwrap();
        let _override = astra_credentials::set_test_credentials_dir(root.path().to_path_buf());
        let scope = scope();
        let variable = "ASTRA_TEST_TERMINAL_PROVIDER_KEY";
        let mut environment = TestEnvironmentVariable::set(variable, "password");
        add(ModelAddArgs {
            name: Some("work".into()),
            base_url: Some(format!("{}/v1", server.uri())),
            provider_model: Some("coding-model".into()),
            context_window: Some(128_000),
            max_output_tokens: Some(8_192),
            credential_env: Some(variable.into()),
            no_auth: false,
            store_secret: false,
        })
        .unwrap();

        let probe_key_path = scope
            .root()
            .join("model-secrets")
            .join("probe_fingerprint_key_v1");
        let first_scope = scope.clone();
        let first = tokio::spawn(async move {
            super::check(
                &first_scope,
                ModelCheckArgs {
                    name: "work".to_string(),
                },
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if server.received_requests().await.unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the old-generation provider request must start");
        let first_key_material = scope
            .secrets()
            .resolve(&LocalCredentialRef::ProtectedFile {
                secret_id: "probe_fingerprint_key_v1".into(),
            })
            .unwrap()
            .unwrap();
        let first_key = std::fs::read_to_string(&probe_key_path).unwrap();
        let binding_revision = scope.models().load().unwrap().models["work"].binding_revision;
        std::fs::remove_file(&probe_key_path).unwrap();

        environment.replace("other-password");
        let second = super::check(
            &scope,
            ModelCheckArgs {
                name: "work".to_string(),
            },
        )
        .await
        .expect("the new-generation provider check must succeed");
        let second: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert!(second["probe_persisted"].as_bool().unwrap());
        let second_key = std::fs::read_to_string(&probe_key_path).unwrap();
        assert_ne!(first_key, second_key);

        // A successful result from the old key generation is rejected too;
        // the guard is symmetric for late success and late failure.
        let old_credential = ResolvedLocalCredential::from_environment(
            &LocalCredentialRef::Environment {
                name: variable.into(),
            },
            |_| Some("password".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            persist_probe_state(
                &scope,
                "work",
                binding_revision,
                LocalModelProbeState::Passed {
                    checked_at_unix_ms: now_unix_ms(),
                    credential_fingerprint: Some(
                        old_credential.fingerprint_with_key(&first_key_material),
                    ),
                },
            )
            .unwrap(),
            ProbePersistOutcome::ProbeIdentityChanged
        );

        let first = first
            .await
            .unwrap()
            .expect_err("a delayed result from the old key generation must not be persisted");
        assert!(first.contains("credential or probe key changed"), "{first}");
        let listed: serde_json::Value =
            serde_json::from_str(&super::list(&scope).unwrap()).unwrap();
        assert_eq!(listed["models"][0]["provider_probe"], "stream_verified");
        assert_eq!(listed["models"][0]["status"], "ready");
    }

    struct TestEnvironmentVariable {
        name: String,
        previous: Option<String>,
    }

    impl TestEnvironmentVariable {
        fn set(name: &str, value: &str) -> Self {
            let previous = std::env::var(name).ok();
            // Tests are serialized because process environment is shared.
            unsafe { std::env::set_var(name, value) };
            Self {
                name: name.to_string(),
                previous,
            }
        }

        fn replace(&mut self, value: &str) {
            unsafe { std::env::set_var(&self.name, value) };
        }
    }

    impl Drop for TestEnvironmentVariable {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(&self.name, value) },
                None => unsafe { std::env::remove_var(&self.name) },
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn probe_rejects_eof_without_finish_reason_or_done_marker() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n",
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
        assert!(error.contains("provider probe failed"), "{error}");
        let saved: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert_eq!(saved["provider_probe"], "failed");
    }

    #[tokio::test]
    #[serial]
    async fn probe_rejects_malformed_choice_even_when_done_is_present() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{}]}\n\ndata: [DONE]\n\n",
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
        assert!(error.contains("provider probe failed"), "{error}");
        let saved: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert_eq!(saved["provider_probe"], "failed");
    }

    #[tokio::test]
    #[serial]
    async fn tui_test_and_use_persists_the_successful_probe_on_apply() {
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
        let scope = scope();
        let mut candidate = prepare_from_tui(
            &scope,
            "work".into(),
            format!("{}/v1", server.uri()),
            "coding-model".into(),
            128_000,
            8_192,
            LocalModelCredentialInput::None,
        )
        .unwrap();
        candidate.check().await.unwrap();
        let applied: serde_json::Value = serde_json::from_str(&candidate.apply().unwrap()).unwrap();
        assert_eq!(applied["provider_probe"], "stream_verified");
        assert!(applied["probe_checked_at_unix_ms"].as_u64().unwrap() > 0);

        let saved: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert_eq!(saved["provider_probe"], "stream_verified");
        assert_eq!(saved["status"], "ready");
        assert!(saved["probe_checked_at_unix_ms"].as_u64().unwrap() > 0);
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
        let saved: serde_json::Value =
            serde_json::from_str(&show("work").unwrap().unwrap()).unwrap();
        assert_eq!(saved["provider_probe"], "failed");
        assert_eq!(saved["probe_failure_code"], "http_401");
        assert_eq!(saved["status"], "needs_attention");
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
        prepare_from_tui(
            &scope(),
            "work".to_string(),
            format!("{}/v1", provider.uri()),
            "coding-model".to_string(),
            128_000,
            8_192,
            LocalModelCredentialInput::Stored("secret-canary".to_string()),
        )
        .and_then(|candidate| candidate.apply())
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
