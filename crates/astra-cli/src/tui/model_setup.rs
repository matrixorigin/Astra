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
    ApiKey(String),
    Connect(Option<String>),
    Models(String),
    Manage,
    Create(UserModelCreateRequestData),
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
    if owner.native_binding.is_some() {
        pane.push_view(Box::new(super::bottom_pane::info_view::InfoView::from_plain("Model access", vec!["Your models are managed by this deployment. Use /model to select an available model.".into()])));
        return;
    }

    pane.pending_model_setup_read = Some(action);
}

pub(crate) fn show_sources(pane: &mut BottomPane, chatgpt_available: bool) {
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
    let owner = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
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
                Ok(ViewResult::ModelSetup(Action::Create(
                    UserModelCreateRequestData {
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
                )))
            },
        )
        .with_initial_values(values),
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
    Saved {
        model_id: String,
        catalog: Result<Vec<astra_services::ModelListItemResponse>, String>,
    },
    Disconnected,
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
    if owner.native_binding.is_some() {
        return Err("This deployment manages your model access through MOI. Your existing model choices are unchanged.".into());
    }
    let api = api.without_bearer_provider();
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
        Action::Create(request) => {
            begin_write()?;
            let payload = json!({"name":request.name,"provider":request.provider,"model":request.model,
                "connection_id":request.connection_id,"api_key":request.api_key,"base_url":request.base_url,
                "inference_protocol":request.inference_protocol,"responses_reasoning_history":request.responses_reasoning_history,
                "context_window":request.context_window,"is_default":false});
            let body = api
                .post_bearer_path_json_text(&token, astra_thin_client::paths::ME_MODELS, &payload)
                .await
                .map_err(map_thin_err)?;
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
            result: Some(ViewResult::ModelSetup(Action::Create(request))),
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
}
