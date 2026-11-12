//! Personal model setup projects the existing model/connection services into TUI views.
use super::bottom_pane::{
    BottomPane,
    form_view::{FieldKind, FieldSpec, FormView},
    list_selection_view::{ListSelectionView, SelectionItem},
    view::ViewResult,
};
use astra_services::models::UserModelCreateRequestData;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Sources,
    DeviceCreate {
        connection_id: String,
        model: String,
        name: String,
        context_window: u32,
        output_limit: u32,
        owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
    },
    ApiKey(String),
    Connect(Option<String>),
    Models(String),
    Manage,
    Create {
        request: UserModelCreateRequestData,
        owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
    },
    Subscription {
        connection_id: String,
        model: String,
    },
    Disconnect(String),
    Activate(String),
    MakeDefault(String),
    ConfirmRemove {
        model_id: String,
        name: String,
    },
    Remove(String),
}

pub(crate) fn choices(
    pane: &mut BottomPane,
    title: &str,
    rows: Vec<(String, Option<String>, Action)>,
) {
    let (items, results): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .map(|(name, description, action)| {
            (
                SelectionItem {
                    name,
                    description,
                    is_current: false,
                },
                ViewResult::ModelSetup(action),
            )
        })
        .unzip();
    pane.push_view(Box::new(
        ListSelectionView::new(items, Some(title.into())).with_results(results),
    ));
}

pub(crate) fn sources(pane: &mut BottomPane) {
    open(pane, Action::Sources);
}

pub(crate) fn open(pane: &mut BottomPane, action: Action) {
    let owner = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    if owner.server_account_id.is_none() {
        // Only navigation intent survives authentication. Never replay a
        // credential-bearing form or a connection mutation under a new owner.
        pane.model_setup_after_login = Some(match action {
            Action::Manage => Action::Manage,
            _ => Action::Sources,
        });
        if crate::cli::native_auth::active().is_some() {
            pane.push_view(Box::new(
                ListSelectionView::new(
                    vec![SelectionItem {
                        name: "Sign in with MOI".into(),
                        description: Some(
                            "Sign in to your platform account, then choose a model source".into(),
                        ),
                        is_current: false,
                    }],
                    Some("MOI sign-in required".into()),
                )
                .with_results(vec![ViewResult::InsertCommand("/login".into())]),
            ));
            return;
        }
        let items = vec![
            SelectionItem {
                name: "Create a local account".into(),
                description: Some("Register and sign in on this Astra deployment".into()),
                is_current: false,
            },
            SelectionItem {
                name: "Sign in".into(),
                description: Some("Use an existing account".into()),
                is_current: false,
            },
        ];
        pane.push_view(Box::new(
            ListSelectionView::new(items, Some("Sign in to configure your models".into()))
                .with_results(vec![
                    ViewResult::InsertCommand("/register".into()),
                    ViewResult::InsertCommand("/login".into()),
                ]),
        ));
        return;
    }
    pane.pending_model_setup_read = Some(action);
}

pub(crate) fn resume_after_login(pane: &mut BottomPane) {
    if let Some(action) = pane.model_setup_after_login.take() {
        open(pane, action);
    }
}

pub(crate) fn show_sources(pane: &mut BottomPane, chatgpt_available: bool) {
    if crate::cli::native_auth::active().is_some() {
        choices(pane, "MOI models · personal device source", vec![
            ("Continue with ChatGPT · this device".into(), Some("Uses your ChatGPT plan. OpenAI credentials and traffic stay on this computer; keep Astra open.".into()), Action::Connect(None)),
            ("Manage my device ChatGPT".into(), Some("Choose a saved account or model, reconnect, or disconnect".into()), Action::Manage),
        ]);
        return;
    }

    let mut rows = vec![(
        "Use my API key".into(),
        Some("Personal credentials, encrypted on this Astra deployment".into()),
        Action::ApiKey(String::new()),
    )];
    if chatgpt_available {
        rows.push((
            "Continue with ChatGPT".into(),
            Some("Use your ChatGPT account and its plan usage".into()),
            Action::Connect(None),
        ));
    }
    rows.push((
        "Manage my models and connections".into(),
        Some("Defaults, removal and ChatGPT connections".into()),
        Action::Manage,
    ));
    choices(pane, "Your model sources", rows);
}

pub(crate) fn unavailable_message(chatgpt: &serde_json::Value) -> String {
    if chatgpt["detail"].is_string() {
        crate::cli::cli_config::cli_utils::read_api_error(503, &chatgpt.to_string())
    } else {
        "ChatGPT connections are unavailable on this Server. Use an API key model, or ask the Server operator to check its version and ChatGPT configuration.".into()
    }
}

pub(crate) fn api_key_form(pane: &mut BottomPane, provider: &str) {
    if provider.is_empty() {
        choices(
            pane,
            "API provider",
            ["openai", "anthropic", "deepseek", "openai-compatible"]
                .into_iter()
                .map(|provider| (provider.into(), None, Action::ApiKey(provider.into())))
                .collect(),
        );
        return;
    }
    model_form(pane, provider.to_owned(), None, None);
}

pub(crate) fn model_form(
    pane: &mut BottomPane,
    provider: String,
    connection: Option<String>,
    model: Option<String>,
) {
    model_form_with_values(pane, provider, connection, model, None, None);
}

pub(crate) fn retry_model_form(
    pane: &mut BottomPane,
    request: UserModelCreateRequestData,
    owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
) {
    model_form_with_values(
        pane,
        request.provider.clone(),
        request.connection_id.clone(),
        Some(request.model.clone()),
        Some(request),
        Some(owner),
    );
}

fn model_form_with_values(
    pane: &mut BottomPane,
    provider: String,
    connection: Option<String>,
    model: Option<String>,
    previous: Option<UserModelCreateRequestData>,
    owner: Option<crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
) {
    let subscription = connection.is_some();
    let compatible = provider == "openai-compatible";
    let mut fields = vec![
        FieldSpec {
            label: "Name",
            kind: FieldKind::Plain,
        },
        FieldSpec {
            label: "Model ID",
            kind: FieldKind::Plain,
        },
        FieldSpec {
            label: "Context tokens",
            kind: FieldKind::Plain,
        },
    ];
    let mut values = vec![
        model.clone().unwrap_or_default(),
        model.unwrap_or_default(),
        "128000".into(),
    ];
    if !subscription {
        fields.push(FieldSpec {
            label: "API key",
            kind: FieldKind::Secret,
        });
        values.push(String::new());
    }
    if compatible {
        fields.push(FieldSpec {
            label: "API base URL",
            kind: FieldKind::Plain,
        });
        values.push(String::new());
    }
    let choose_format = !subscription && provider != "anthropic";
    if choose_format {
        fields.push(FieldSpec {
            label: "API format (chat or responses)",
            kind: FieldKind::Plain,
        });
        values.push("chat".into());
    }
    if let Some(request) = previous {
        values[0] = request.name;
        values[1] = request.model;
        values[2] = request.context_window.to_string();
        if !subscription {
            values[3] = request.api_key;
        }
        if compatible {
            values[4] = request.base_url.unwrap_or_default();
        }
        if choose_format
            && request.inference_protocol
                == Some(astra_core::model_wire::LlmProviderProtocol::OpenAiResponses)
        {
            *values.last_mut().expect("format field exists") = "responses".into();
        }
    }
    let owner = owner.unwrap_or_else(crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot);
    let form_owner = owner.clone();
    pane.push_view(Box::new(
        FormView::new(
            "Personal model · stored encrypted on this Astra deployment",
            fields,
            move |values| {
                if !owner.is_current() {
                    return Err("Account changed; reopen /model".into());
                }
                let context_window = values[2]
                    .parse::<i32>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or("Context tokens must be a positive number")?;
                Ok(ViewResult::ModelSetup(Action::Create {
                    request: UserModelCreateRequestData {
                        connection_id: connection.clone(),
                        responses_reasoning_history: None,
                        name: values[0].trim().into(),
                        provider: provider.clone(),
                        inference_protocol: if choose_format {
                            match values
                                .last()
                                .map(|value| value.trim().to_ascii_lowercase())
                                .as_deref()
                            {
                                Some("chat") => None,
                                Some("responses") => Some(
                                    astra_core::model_wire::LlmProviderProtocol::OpenAiResponses,
                                ),
                                _ => return Err("API format must be chat or responses".into()),
                            }
                        } else {
                            None
                        },
                        model: values[1].trim().into(),
                        context_window,
                        api_key: if subscription {
                            String::new()
                        } else {
                            values[3].clone()
                        },
                        base_url: compatible.then(|| values[4].trim().into()),
                        is_default: false,
                    },
                    owner: owner.clone(),
                }))
            },
        )
        .with_initial_values(values)
        .while_valid(move || form_owner.is_current()),
    ));
}

pub(crate) enum Outcome {
    Sources(serde_json::Value),
    Connected {
        connection_id: String,
        status: String,
    },
    Connections(
        serde_json::Value,
        Vec<astra_services::models::UserModelRecord>,
    ),
    Models {
        connection_id: String,
        slugs: Vec<String>,
        existing: Vec<(String, String)>,
    },
    DeviceConnections {
        connections: Vec<astra_credentials::chatgpt::Connection>,
        models: Vec<(String, String)>,
    },
    #[cfg(unix)]
    DeviceSaved {
        offering_id: String,
        catalog: Result<Vec<astra_services::ModelListItemResponse>, String>,
        runner: crate::cli::device_models::DeviceRunner,
    },
    Saved {
        model_id: String,
        catalog: Result<Vec<astra_services::ModelListItemResponse>, String>,
    },
    CreateRejected {
        request: UserModelCreateRequestData,
        message: String,
    },
    Disconnected,
    DeviceDisconnected {
        revocation_confirmed: bool,
    },
    Removed,
}

pub(crate) fn current_model_is_usable(
    model: Option<&crate::cli::session::session_state::SessionModelChoice>,
    catalog: &[astra_services::ModelListItemResponse],
) -> bool {
    use crate::cli::session::{session_runtime, session_state::SessionModelChoice};
    match model {
        Some(SessionModelChoice::Selected(selection)) => catalog.iter().any(|row| {
            row.offering_id == selection.offering_id
                && session_runtime::model_list_entry_is_active(row)
        }),
        Some(SessionModelChoice::Requested(name)) => {
            session_runtime::resolve_server_model_selection_from_catalog(name, catalog).is_ok()
        }
        None => false,
    }
}

pub(crate) async fn run(
    action: Action,
    api: astra_thin_client::ThinClient,
    owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
    cancel: tokio_util::sync::CancellationToken,
    phase: std::sync::Arc<super::login_control::LoginControl>,
    progress: tokio::sync::mpsc::UnboundedSender<crate::cli::model_connections::ConnectProgress>,
) -> Result<Outcome, String> {
    use crate::cli::session::session_runtime::owner_access_token;
    use crate::cli::{
        cli_config::{cli_args::ModelConnectArgs, cli_utils::map_thin_err},
        model_connections,
    };
    use serde_json::{Value, json};
    if let Action::Create {
        owner: form_owner, ..
    } = &action
        && (form_owner != &owner || !form_owner.is_current())
    {
        return Err("Account changed; reopen /model".into());
    }
    if owner.native_binding.is_some() {
        #[cfg(unix)]
        {
            return run_native(action, api, owner, cancel, phase, progress).await;
        }
        #[cfg(not(unix))]
        {
            return Err(
                "Device ChatGPT requires Linux or macOS; organizational models remain available."
                    .into(),
            );
        }
    }
    let api = api.without_bearer_provider();
    let preflight: Result<(String, Value), String> = async {
    let token = tokio::select! {
        _ = cancel.cancelled() => return Err("Model setup cancelled".into()),
        token = owner_access_token(&api, &owner, None) => token.ok_or("Sign in before configuring a personal model")?,
    };
    if !owner.is_current() {
        return Err("Account changed; reopen /model".into());
    }
    let body = tokio::select! {
        _ = cancel.cancelled() => return Err("Model setup cancelled".into()),
        result = tokio::time::timeout(std::time::Duration::from_secs(15), api.get_bearer_path_query_text(&token, model_connections::CONNECTIONS, &[])) =>
            result.map_err(|_| "Loading model sources timed out; reopen /model to retry")?.map_err(map_thin_err)?,
    };
    let availability: Value =
        serde_json::from_str(&body).map_err(|_| "Invalid model management response")?;
    if availability["management_available"] != true {
        return Err("Personal model configuration is unavailable for this account.".into());
    }
    Ok((token, availability))
    }.await;
    let (token, availability) = match preflight {
        Ok(result) => result,
        Err(message) => {
            if !cancel.is_cancelled()
                && owner.is_current()
                && let Action::Create { request, .. } = action
            {
                return Ok(Outcome::CreateRejected {
                    request,
                    message: format!("Model setup failed; the model was not saved. {message}"),
                });
            }
            return Err(message);
        }
    };
    if matches!(action, Action::Sources) {
        return Ok(Outcome::Sources(availability["chatgpt"].clone()));
    }
    if matches!(action, Action::Connect(_)) && availability["chatgpt"]["available"] != true {
        return Err(unavailable_message(&availability["chatgpt"]));
    }
    let begin_write = || {
        if !owner.is_current() {
            return Err("Account changed; reopen /model".into());
        }
        phase.begin_exchange()
    };
    let model_id = match action {
        Action::DeviceCreate { .. } => return Err("Device ChatGPT requires a MOI login".into()),
        Action::Connect(connection) => {
            let linked = model_connections::connect_with_progress(
                &api,
                &token,
                ModelConnectArgs {
                    provider: "chatgpt".into(),
                    label: None,
                    connection,
                    retry_authorization_id: None,
                },
                owner.profile_name.as_deref(),
                cancel,
                |event| {
                    if matches!(event, model_connections::ConnectProgress::Completing) {
                        if !owner.is_current() {
                            return Err("Account changed during authorization".into());
                        }
                        begin_write()?;
                    }
                    let _ = progress.send(event);
                    Ok(())
                },
            )
            .await?;
            let id = linked["connection"]["connection_id"]
                .as_str()
                .ok_or("Connection ID missing")?
                .to_owned();
            return Ok(Outcome::Connected {
                connection_id: id,
                status: linked["connection"]["status"]
                    .as_str()
                    .ok_or("Connection status missing")?
                    .to_owned(),
            });
        }
        Action::Models(id) => {
            let path = model_connections::connection_path(&id, "models")?;
            let body = tokio::select! {
                _ = cancel.cancelled() => return Err("Model selection cancelled. Your account remains connected.".into()),
                result = api.get_bearer_path_query_text(&token, &path, &[]) =>
                    result.map_err(|error| format!("{}; retry /model → Manage → Choose models. Your account remains connected.", map_thin_err(error)))?,
            };
            let catalog: Value =
                serde_json::from_str(&body).map_err(|_| "Invalid subscription model catalog")?;
            let slugs: Vec<_> = catalog["items"]
                .as_array()
                .ok_or("Invalid subscription model catalog")?
                .iter()
                .filter_map(|row| row["slug"].as_str().map(str::to_owned))
                .collect();
            if slugs.is_empty() {
                return Err("ChatGPT connected, but this account exposes no usable models. Use /model to choose another source.".into());
            }
            let existing = catalog["configured_models"]
                .as_array()
                .ok_or("Invalid configured model list")?
                .iter()
                .filter_map(|model| {
                    Some((
                        model["model_id"].as_str()?.to_owned(),
                        model["name"].as_str()?.to_owned(),
                    ))
                })
                .collect();
            return Ok(Outcome::Models {
                connection_id: id,
                slugs,
                existing,
            });
        }
        Action::Create { request, .. } => {
            begin_write()?;
            let payload = json!({"name":request.name,"provider":request.provider,"model":request.model,
                "connection_id":request.connection_id,"api_key":request.api_key,"base_url":request.base_url,
                "inference_protocol":request.inference_protocol,"responses_reasoning_history":request.responses_reasoning_history,
                "context_window":request.context_window,"is_default":false});
            let body = match api
                .post_bearer_path_json_text(&token, astra_thin_client::paths::ME_MODELS, &payload)
                .await
            {
                Ok(body) => body,
                Err(error)
                    if crate::cli::cli_config::cli_utils::model_creation_definitely_rejected(
                        &error,
                    ) =>
                {
                    return Ok(Outcome::CreateRejected {
                        request,
                        message: crate::cli::cli_config::cli_utils::model_creation_error(error),
                    });
                }
                Err(error) => {
                    return Err(crate::cli::cli_config::cli_utils::model_creation_error(
                        error,
                    ));
                }
            };
            let created: Value =
                serde_json::from_str(&body).map_err(|_| "Invalid model creation response")?;
            let id = created["model_id"]
                .as_str()
                .ok_or("Model ID missing")?
                .to_owned();
            Ok::<_, String>(id)
        }
        Action::Activate(id) => {
            begin_write()?;
            api.put_bearer_path_json_text(
                &token,
                &astra_thin_client::paths::me_model(&id),
                &json!({"is_active":true}),
            )
            .await
            .map_err(map_thin_err)?;
            Ok::<_, String>(id)
        }
        Action::MakeDefault(id) => {
            begin_write()?;
            api.put_bearer_path_json_text(
                &token,
                &astra_thin_client::paths::me_model(&id),
                &json!({"is_default":true}),
            )
            .await
            .map_err(map_thin_err)?;
            Ok::<_, String>(id)
        }
        Action::Remove(id) => {
            begin_write()?;
            api.delete_bearer_path_text(&token, &astra_thin_client::paths::me_model(&id))
                .await
                .map_err(map_thin_err)?;
            return Ok(Outcome::Removed);
        }
        Action::Manage => {
            let body = tokio::select! {
                           _ = cancel.cancelled() => return Err("Model setup cancelled".into()),
                           result = api
                           .get_bearer_path_query_text(&token, astra_thin_client::paths::ME_MODELS, &[])
            => result.map_err(map_thin_err)?,
                       };
            let models: astra_services::models::UserModelListResponse =
                serde_json::from_str(&body).map_err(|_| "Invalid personal model list")?;
            return Ok(Outcome::Connections(availability, models.items));
        }
        Action::Disconnect(id) => {
            begin_write()?;
            api.post_bearer_path_empty_text(
                &token,
                &model_connections::connection_path(&id, "disconnect")?,
            )
            .await
            .map_err(map_thin_err)?;
            return Ok(Outcome::Disconnected);
        }
        _ => Err("Choose a model source in /model".into()),
    }?;
    let catalog = crate::cli::session::session_runtime::fetch_owner_model_catalog(&api, &owner)
        .await
        .map_err(|error| error.to_string());
    Ok(Outcome::Saved { model_id, catalog })
}

#[cfg(test)]
mod tests {
    use super::super::bottom_pane::BottomPaneAction;
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    fn key(pane: &mut BottomPane, code: KeyCode) -> BottomPaneAction {
        pane.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn text(pane: &mut BottomPane, value: &str) {
        for character in value.chars() {
            key(pane, KeyCode::Char(character));
        }
    }
    #[test]
    #[serial_test::serial]
    fn api_key_form_masks_credentials_and_emits_the_personal_request() {
        let mut pane = BottomPane::new();
        pane.replace_composer_text("unsent conversation");
        model_form(&mut pane, "openai".into(), None, None);
        text(&mut pane, "personal");
        key(&mut pane, KeyCode::Tab);
        text(&mut pane, "fixture-model");
        key(&mut pane, KeyCode::Tab);
        key(&mut pane, KeyCode::Tab);
        pane.handle_paste("synthetic-private-key");
        pane.handle_paste("rejected\nsecond-line");
        key(&mut pane, KeyCode::Tab);
        for _ in 0..4 {
            key(&mut pane, KeyCode::Backspace);
        }
        text(&mut pane, "responses");
        assert_eq!(pane.composer.text(), "unsent conversation");
        let area = ratatui::layout::Rect::new(0, 0, 100, 14);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        pane.render(area, &mut buffer);
        let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(!rendered.contains("synthetic-private-key"));
        let BottomPaneAction::ViewCompleted {
            result: Some(ViewResult::ModelSetup(Action::Create { request, .. })),
            ..
        } = key(&mut pane, KeyCode::Enter)
        else {
            panic!("form did not submit");
        };
        assert_eq!(request.name, "personal");
        assert_eq!(request.model, "fixture-model");
        assert_eq!(request.api_key, "synthetic-private-key");
        assert_eq!(request.context_window, 128000);
        assert_eq!(
            request.inference_protocol,
            Some(astra_core::model_wire::LlmProviderProtocol::OpenAiResponses)
        );
        assert!(!request.is_default);
        assert!(!format!("{request:?}").contains("synthetic-private-key"));
        assert_eq!(pane.composer.text(), "unsent conversation");
    }

    #[test]
    #[serial_test::serial]
    fn failed_probe_restores_editable_form_without_exposing_key() {
        let mut pane = BottomPane::new();
        let request = UserModelCreateRequestData {
            connection_id: None,
            name: "personal".into(),
            provider: "openai-compatible".into(),
            inference_protocol: Some(astra_core::model_wire::LlmProviderProtocol::OpenAiResponses),
            responses_reasoning_history: None,
            model: "fixture-model".into(),
            base_url: Some("https://models.example/v1".into()),
            api_key: "synthetic-private-key".into(),
            context_window: 64000,
            is_default: false,
        };
        retry_model_form(
            &mut pane,
            request.clone(),
            crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
        );
        let area = ratatui::layout::Rect::new(0, 0, 100, 16);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        pane.render(area, &mut buffer);
        let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("fixture-model"));
        assert!(!rendered.contains("synthetic-private-key"));
        let BottomPaneAction::ViewCompleted {
            result:
                Some(ViewResult::ModelSetup(Action::Create {
                    request: resubmitted,
                    ..
                })),
            ..
        } = key(&mut pane, KeyCode::Enter)
        else {
            panic!("restored form did not submit");
        };
        assert_eq!(resubmitted, request);
    }

    #[test]
    #[serial_test::serial]
    fn restored_secret_form_is_discarded_when_owner_changes() {
        use crate::cli::cli_config::cli_utils::{
            cli_owner_auth_snapshot, install_cli_profile_identity_for_test,
        };
        let _first =
            install_cli_profile_identity_for_test("model-form-first", Some("first-user")).unwrap();
        let owner = cli_owner_auth_snapshot();
        let mut pane = BottomPane::new();
        retry_model_form(
            &mut pane,
            UserModelCreateRequestData {
                connection_id: None,
                name: "mine".into(),
                provider: "openai".into(),
                inference_protocol: None,
                responses_reasoning_history: None,
                model: "fixture".into(),
                base_url: None,
                api_key: "synthetic-key".into(),
                context_window: 128000,
                is_default: false,
            },
            owner,
        );
        assert!(pane.has_active_view());
        let _second =
            install_cli_profile_identity_for_test("model-form-second", Some("second-user"))
                .unwrap();
        pane.pre_draw_tick(std::time::Instant::now());
        assert!(!pane.has_active_view());
    }
    #[test]
    #[serial_test::serial]
    fn cancelling_setup_preserves_the_conversation_draft() {
        let mut pane = BottomPane::new();
        pane.replace_composer_text("keep this input");
        model_form(
            &mut pane,
            "chatgpt".into(),
            Some("connection-id".into()),
            Some("fixture-model".into()),
        );
        assert!(matches!(
            key(&mut pane, KeyCode::Esc),
            BottomPaneAction::ViewCompleted { result: None, .. }
        ));
        assert_eq!(pane.composer.text(), "keep this input");
    }

    #[test]
    fn source_picker_omits_disabled_chatgpt_and_preserves_api_key_setup() {
        for available in [false, true] {
            let mut pane = BottomPane::new();
            pane.replace_composer_text("unsent draft");
            show_sources(&mut pane, available);
            let area = ratatui::layout::Rect::new(0, 0, 110, 20);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            pane.render(area, &mut buffer);
            let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
            assert_eq!(rendered.contains("Continue with ChatGPT"), available);
            assert!(matches!(
                key(&mut pane, KeyCode::Enter),
                BottomPaneAction::ViewCompleted {
                    result: Some(ViewResult::ModelSetup(Action::ApiKey(_))),
                    ..
                }
            ));
            assert_eq!(pane.composer.text(), "unsent draft");
        }
    }

    #[test]
    #[cfg(unix)]
    fn native_model_setup_preserves_moi_login_intent_and_never_replays_a_mutation() {
        // ACTIVE is process-wide production identity. Match existing native
        // auth tests by isolating this case in its own test process.
        const CHILD: &str = "ASTRA_TEST_NATIVE_DEVICE_MODEL_UI";
        if std::env::var(CHILD).as_deref() != Ok("1") {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tui::model_setup::tests::native_model_setup_preserves_moi_login_intent_and_never_replays_a_mutation", "--nocapture"])
                .env(CHILD, "1").status().unwrap();
            assert!(status.success());
            return;
        }
        use crate::cli::native_auth::{binding_for_test, install_active_for_test, test_session};
        let directory = tempfile::tempdir().unwrap();
        let _credentials =
            astra_credentials::set_test_credentials_dir(directory.path().join("credentials"));
        let store =
            astra_credentials::native::NativeStore::with_directory(directory.path().join("native"));
        let session = store.publish(test_session()).unwrap().0;
        let _active = install_active_for_test(binding_for_test(store.clone(), session));
        crate::cli::cli_config::cli_utils::configure_cli_profile_identity(
            None,
            crate::cli::cli_config::cli_utils::CliProfileIdentityAdmission::RequireBoundAccount,
        )
        .unwrap();
        let mut pane = BottomPane::new();
        pane.replace_composer_text("unsent draft");
        show_sources(&mut pane, false);
        assert!(matches!(
            key(&mut pane, KeyCode::Enter),
            BottomPaneAction::ViewCompleted {
                result: Some(ViewResult::ModelSetup(Action::Connect(None))),
                ..
            }
        ));
        assert_eq!(pane.composer.text(), "unsent draft");
        store.logout().unwrap();
        open(&mut pane, Action::Disconnect("old-connection".into()));
        assert!(matches!(
            pane.model_setup_after_login,
            Some(Action::Sources)
        ));
        assert!(
            matches!(key(&mut pane, KeyCode::Enter), BottomPaneAction::ViewCompleted {
            result: Some(ViewResult::InsertCommand(command)), ..
        } if command == "/login")
        );
        let next = store.publish(test_session()).unwrap().0;
        let _next = install_active_for_test(binding_for_test(store, next));
        resume_after_login(&mut pane);
        assert!(pane.model_setup_after_login.is_none());
        assert!(matches!(
            pane.pending_model_setup_read,
            Some(Action::Sources)
        ));
        assert_eq!(pane.composer.text(), "unsent draft");
    }
}

#[cfg(unix)]
async fn run_native(
    action: Action,
    api: astra_thin_client::ThinClient,
    owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
    cancel: tokio_util::sync::CancellationToken,
    phase: std::sync::Arc<super::login_control::LoginControl>,
    progress: tokio::sync::mpsc::UnboundedSender<crate::cli::model_connections::ConnectProgress>,
) -> Result<Outcome, String> {
    use crate::cli::{device_models::DeviceModels, model_connections::ConnectProgress};
    if !owner.is_current() {
        return Err("MOI account changed; reopen /model".into());
    }
    let binding = owner
        .native_binding
        .clone()
        .ok_or("Sign in to MOI before choosing device ChatGPT")?;
    let device = tokio::select! {
        _ = cancel.cancelled() => return Err("Device model setup cancelled".into()),
        result = DeviceModels::current(binding) => result?,
    };
    match action {
        Action::Sources => Ok(Outcome::Sources(serde_json::json!({"available":true,"execution":"device"}))),
        Action::Connect(existing) => {
            let linked = device.connect(existing, None, cancel, |event| {
                if matches!(event, ConnectProgress::Completing) { phase.begin_exchange()?; }
                let _ = progress.send(event); Ok(())
            }).await?;
            Ok(Outcome::Connected { connection_id: linked.connection_ref.to_string(),
                status: if linked.enabled {"connected"} else {"plan_permission_required"}.into() })
        },
        Action::Models(connection_id) => {
            let slugs = device.models(&connection_id).await?;
            let existing = device.saved().await?.into_iter().filter(|(_, model)| model.connection_ref.to_string() == connection_id)
                .map(|(name, _)| (name.clone(), name)).collect();
            Ok(Outcome::Models { connection_id, slugs, existing })
        },
        Action::Manage => Ok(Outcome::DeviceConnections { connections: device.connections().await?,
            models: device.saved().await?.into_iter().map(|(name, model)| (name, model.model)).collect() }),
        Action::DeviceCreate { connection_id, model, name, context_window, output_limit, owner: form_owner } => {
            if form_owner != owner || !owner.is_current() { return Err("MOI account changed; reopen /model".into()); }
            phase.begin_exchange()?;
            let (offering_id, runner) = device.save(name, connection_id, model, context_window, output_limit, false).await?;
            let catalog = wait_for_device_catalog(&api, &owner, &offering_id).await;
            Ok(Outcome::DeviceSaved { offering_id, catalog, runner })
        },
        Action::Activate(name) => {
            let (_, model) = device.saved().await?.into_iter().find(|(candidate, _)| *candidate == name).ok_or("Device model no longer exists")?;
            phase.begin_exchange()?;
            let (offering_id, runner) = device.save(name, model.connection_ref.to_string(), model.model, model.context_window, model.max_output_tokens, true).await?;
            let catalog = wait_for_device_catalog(&api, &owner, &offering_id).await;
            Ok(Outcome::DeviceSaved { offering_id, catalog, runner })
        },
        Action::Disconnect(id) => {
            phase.begin_exchange()?;
            let revocation_confirmed = device.disconnect(&id).await?;
            Ok(Outcome::DeviceDisconnected { revocation_confirmed })
        },
        Action::Remove(name) => { phase.begin_exchange()?; device.remove(name).await?; Ok(Outcome::Removed) },
        _ => Err("Use the MOI device ChatGPT source or select an organizational model. Server credential configuration is unchanged.".into()),
    }
}

#[cfg(unix)]
async fn wait_for_device_catalog(
    api: &astra_thin_client::ThinClient,
    owner: &crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
    offering: &str,
) -> Result<Vec<astra_services::ModelListItemResponse>, String> {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if !owner.is_current() { return Err("MOI account changed; reopen /model".into()); }
            let catalog = crate::cli::session::session_runtime::fetch_owner_model_catalog(api, owner).await
                .map_err(|error| error.to_string())?;
            if catalog.iter().any(|row| row.offering_id == offering && row.is_active) { return Ok(catalog); }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }).await.map_err(|_| "Model saved on this device; Server admission is not ready. Reopen /model to retry without reconnecting ChatGPT".to_string())?
}

pub(crate) fn device_model_form(pane: &mut BottomPane, connection_id: String, model: String) {
    let owner = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    let form_owner = owner.clone();
    let model_id = model.clone();
    pane.push_view(Box::new(
        FormView::new(
            "My ChatGPT · credentials stay on this computer",
            vec![
                FieldSpec {
                    label: "Name",
                    kind: FieldKind::Plain,
                },
                FieldSpec {
                    label: "Context limit (tokens)",
                    kind: FieldKind::Plain,
                },
                FieldSpec {
                    label: "Output budget (tokens)",
                    kind: FieldKind::Plain,
                },
            ],
            move |values| {
                if !owner.is_current() {
                    return Err("MOI account changed; reopen /model".into());
                }
                let context_window = values[1]
                    .parse::<u32>()
                    .map_err(|_| "Context limit must be a positive number")?;
                let output_limit = values[2]
                    .parse::<u32>()
                    .map_err(|_| "Output budget must be a positive number")?;
                if values[0].is_empty()
                    || context_window == 0
                    || context_window > i32::MAX as u32
                    || output_limit == 0
                    || output_limit > context_window
                {
                    return Err(
                        "Choose a name and positive token budgets; output must fit the context"
                            .into(),
                    );
                }
                Ok(ViewResult::ModelSetup(Action::DeviceCreate {
                    connection_id: connection_id.clone(),
                    model: model_id.clone(),
                    name: values[0].clone(),
                    context_window,
                    output_limit,
                    owner: owner.clone(),
                }))
            },
        )
        .with_initial_values(vec![model, "128000".into(), "4096".into()])
        .while_valid(move || form_owner.is_current()),
    ));
}
