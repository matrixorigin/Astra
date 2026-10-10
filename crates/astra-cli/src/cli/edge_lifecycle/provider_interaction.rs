use crate::cli::chat_stream;
#[cfg(test)]
use serde_json::Value;

/// Invocation-local transport to the existing durable interaction owner.
/// No local question ledger or provider principal is created here.
pub(crate) struct NativeInvocationInteractionGate {
    pub(crate) api: astra_thin_client::ThinClient,
    /// Session-scoped authentication owned by the native delivery lifecycle.
    /// An invocation may outlive a token refresh while waiting for the user;
    /// reading it at each HTTP boundary avoids retaining an expired copy.
    pub(crate) auth: std::sync::Arc<tokio::sync::RwLock<String>>,
    pub(crate) auth_provider: Option<std::sync::Arc<dyn astra_thin_client::client::BearerProvider>>,
    pub(crate) edge_transport_id: std::sync::Arc<tokio::sync::RwLock<String>>,
    pub(crate) edge_agent_id: String,
    pub(crate) physical_workspace_id: String,
    pub(crate) identity: astra_turn_types::ToolInvocationIdentity,
    pub(crate) provider: astra_turn_core::provider_resolution::NativeCollaboratorProtocol,
    pub(crate) deadline: std::time::Instant,
    pub(crate) ask_user_request_tx: Option<chat_stream::AskUserRequestTx>,
}

impl NativeInvocationInteractionGate {
    async fn auth_token(&self) -> Result<String, astra_thin_client::ThinClientError> {
        if let Some(provider) = &self.auth_provider {
            provider.token().await
        } else {
            Ok(self.auth.read().await.clone())
        }
    }

    async fn answer_question(
        &self,
        interaction: &astra_turn_types::ProviderInteractionRequest,
    ) -> Result<(), astra_thin_client::ThinClientError> {
        use astra_thin_client::{ProviderInteractionRespondRequest, ThinClientError};
        let tx = self.ask_user_request_tx.as_ref().ok_or_else(|| {
            ThinClientError::InvalidInput("native question has no interactive consumer".into())
        })?;
        let mut prompt =
            crate::edge_tools::native_codex::question_prompt(interaction).map_err(|_| {
                ThinClientError::InvalidInput(
                    "native question cannot be represented by this UI".into(),
                )
            })?;
        prompt.context = Some(format!(
            "{} · session {} · run {}",
            self.provider.display_name(),
            self.identity.session_id,
            self.identity.run_id
        ));
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        chat_stream::enqueue_interactive_request(
            tx,
            chat_stream::AskUserRequest {
                prompt: prompt.clone(),
                response_tx,
            },
        )
        .map_err(|_| {
            ThinClientError::InvalidInput("native question consumer is unavailable or busy".into())
        })?;
        let response =
            tokio::time::timeout_at(tokio::time::Instant::from_std(self.deadline), response_rx)
                .await
                .map_err(|_| ThinClientError::AdmissionDeadlineExpired)?
                .map_err(|_| {
                    ThinClientError::InvalidInput("native question consumer closed".into())
                })?;
        let (cancelled, payload) = match response {
            chat_stream::AskUserResponse::Submitted(answers) => (
                false,
                Some(
                    crate::edge_tools::native_codex::question_response(
                        interaction,
                        &prompt,
                        &answers,
                    )
                    .map_err(|_| {
                        ThinClientError::InvalidInput("native question response is invalid".into())
                    })?,
                ),
            ),
            chat_stream::AskUserResponse::Cancelled => (true, None),
        };
        let body = ProviderInteractionRespondRequest {
            request_id: interaction.request_id.clone(),
            session_id: self.identity.session_id.clone(),
            run_id: self.identity.run_id.clone(),
            cancelled,
            payload,
        };
        let auth = self.auth_token().await?;
        tokio::time::timeout_at(
            tokio::time::Instant::from_std(self.deadline),
            self.api
                .post_provider_interaction_response(Some(&auth), &body),
        )
        .await
        .map_err(|_| ThinClientError::AdmissionDeadlineExpired)??;
        Ok(())
    }
}

#[async_trait::async_trait]
impl astra_tools::ProviderInteractionGate for NativeInvocationInteractionGate {
    async fn request_interaction(
        &self,
        interaction: &astra_turn_types::ProviderInteractionRequest,
    ) -> astra_tools::ProviderInteractionDecision {
        use astra_tools::ProviderInteractionDecision;
        use astra_turn_types::ProviderInteractionOutcome;
        let remaining = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        let timeout = interaction
            .timeout_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(remaining)
            .min(remaining);
        if timeout.is_zero() {
            return ProviderInteractionDecision::Timeout;
        }
        let body = astra_thin_client::ToolInteractionRequest {
            identity: self.identity.clone(),
            edge_agent_id: self.edge_agent_id.clone(),
            physical_workspace_id: self.physical_workspace_id.clone(),
            interaction: interaction.clone(),
        };
        let (required_tx, mut required_rx) = tokio::sync::mpsc::channel(1);
        let edge_transport_id = self.edge_transport_id.read().await.clone();
        let auth = match self.auth_token().await {
            Ok(auth) => auth,
            Err(error) => return native_interaction_error(error),
        };
        let receive = self.api.post_tool_interaction_request(
            Some(&auth),
            &edge_transport_id,
            &body,
            timeout,
            required_tx,
        );
        tokio::pin!(receive);
        let answer = async {
            required_rx.recv().await.ok_or_else(|| {
                astra_thin_client::ThinClientError::InvalidProviderInteractionResponse(
                    "interaction stream closed before a question".into(),
                )
            })?;
            self.answer_question(interaction).await
        };
        tokio::pin!(answer);
        let mut answered = false;
        let result = loop {
            tokio::select! {
                biased;
                result = &mut receive => break result,
                result = &mut answer, if !answered => {
                    answered = true;
                    if let Err(error) = result { return native_interaction_error(error); }
                }
            }
        };
        match result {
            Ok(response) => match response.outcome {
                ProviderInteractionOutcome::Submitted => response
                    .payload
                    .map(ProviderInteractionDecision::Submitted)
                    .unwrap_or_else(|| {
                        ProviderInteractionDecision::Error(
                            "durable interaction response missing payload".into(),
                        )
                    }),
                ProviderInteractionOutcome::Cancelled => ProviderInteractionDecision::Cancelled,
                ProviderInteractionOutcome::TimedOut => ProviderInteractionDecision::Timeout,
            },
            Err(error) => native_interaction_error(error),
        }
    }
}

fn native_interaction_error(
    error: astra_thin_client::ThinClientError,
) -> astra_tools::ProviderInteractionDecision {
    use astra_thin_client::ThinClientError as E;
    use astra_tools::ProviderInteractionDecision as D;
    // Never format the error: transport URLs, response bodies and auth values are private.
    let category = match error {
        E::AdmissionDeadlineExpired => return D::Timeout,
        E::Http(error) if error.is_timeout() => return D::Timeout,
        E::Http(error) if error.is_decode() => "protocol",
        E::Http(_) => "network",
        E::InvalidAuthHeader => "authentication",
        E::Api { status, .. } => {
            let category = match status.as_u16() {
                401 | 403 => "authentication",
                408 | 504 => "deadline",
                _ => "server rejection",
            };
            return D::Error(format!(
                "native interaction {category} (HTTP {})",
                status.as_u16()
            ));
        }
        E::InvalidBaseUrl(_) | E::InvalidInput(_) => "configuration",
        E::SessionCancellationPending { .. } => "cancellation pending",
        E::SseParse(_)
        | E::IncompatibleRuntime { .. }
        | E::InvalidProviderInteractionResponse(_)
        | E::ResponseTooLarge { .. }
        | E::InvalidSessionCancellationResponse(_)
        | E::InvalidSseJson(_)
        | E::Json(_) => "protocol",
    };
    D::Error(format!("native interaction {category} failure"))
}

#[cfg(test)]
mod native_interaction_gate_tests {
    use super::*;
    use astra_tools::{ProviderInteractionDecision as Decision, ProviderInteractionGate};
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // HTTP dependency fixture, not a replacement Run/ledger implementation.
    // Durable registration and resolution have separate real-DB coverage.
    #[tokio::test]
    async fn native_question_is_answered_while_its_request_stream_remains_open() {
        use astra_thin_client::{ProviderInteractionRespondRequest, ToolInteractionRequest};
        use axum::{Json, Router, extract::State, routing::post};
        struct Fixture {
            request: ToolInteractionRequest,
            answer: tokio::sync::Mutex<Option<Value>>,
            ready: tokio::sync::Notify,
        }
        async fn request(
            State(state): State<std::sync::Arc<Fixture>>,
            Json(body): Json<ToolInteractionRequest>,
        ) -> axum::response::Sse<
            impl futures_util::Stream<
                Item = Result<axum::response::sse::Event, std::convert::Infallible>,
            >,
        > {
            assert_eq!(body, state.request);
            let stream = futures_util::stream::unfold(
                (state, body, 0u8),
                |(state, body, phase)| async move {
                    if phase == 2 {
                        return None;
                    }
                    let event = if phase == 0 {
                        let required = serde_json::json!({
                            "type":"provider_interaction_required", "index":3,
                            "run_id":body.identity.run_id, "request_id":body.interaction.request_id,
                            "interaction":body.interaction,
                            "tool_invocation_origin":{"identity":body.identity,"edge_agent_id":body.edge_agent_id}
                        });
                        required
                    } else {
                        state.ready.notified().await;
                        let response = state.answer.lock().await.take().unwrap();
                        serde_json::json!({"type":"tool_interaction_response", "response":response})
                    };
                    Some((
                        Ok(axum::response::sse::Event::default().data(event.to_string())),
                        (state, body, phase + 1),
                    ))
                },
            );
            axum::response::Sse::new(stream)
        }
        async fn respond(
            State(state): State<std::sync::Arc<Fixture>>,
            headers: axum::http::HeaderMap,
            Json(body): Json<ProviderInteractionRespondRequest>,
        ) -> Json<Value> {
            assert_eq!(
                headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer rotated-token")
            );
            assert_eq!(body.request_id, state.request.interaction.request_id);
            assert_eq!(body.run_id, state.request.identity.run_id);
            assert_eq!(body.session_id, state.request.identity.session_id);
            assert!(!body.cancelled);
            assert_eq!(
                body.payload,
                Some(serde_json::json!({"answers":{"q":{"answers":["A"]}}}))
            );
            *state.answer.lock().await = Some(serde_json::json!({
                "request_id":body.request_id, "outcome":"submitted", "payload":body.payload
            }));
            state.ready.notify_one();
            Json(serde_json::json!({"accepted":true}))
        }
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            "account", "session", "run", "chain", "call",
        )
        .unwrap();
        let interaction = astra_turn_types::ProviderInteractionRequest {
            request_id: "rpc".into(),
            timeout_ms: Some(5000),
            provider_stage_input_id: None,
            payload: serde_json::json!({"provider":"codex", "method":"item/tool/requestUserInput",
                "params":{"questions":[{"id":"q", "header":"Direction", "question":"Choose a direction", "isOther":true}]}}),
        };
        let fixture = std::sync::Arc::new(Fixture {
            request: ToolInteractionRequest {
                identity: identity.clone(),
                edge_agent_id: "agent".into(),
                physical_workspace_id: "physical-test".into(),
                interaction: interaction.clone(),
            },
            answer: tokio::sync::Mutex::new(None),
            ready: tokio::sync::Notify::new(),
        });
        let router = Router::new()
            .route("/tools/interactions/request", post(request))
            .route("/provider-interactions/respond", post(respond))
            .with_state(fixture);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let gate = NativeInvocationInteractionGate {
            api: astra_thin_client::ThinClient::new(&format!("http://{address}"), None).unwrap(),
            auth: std::sync::Arc::new(tokio::sync::RwLock::new("fixture-token".into())),
            auth_provider: None,
            edge_transport_id: std::sync::Arc::new(tokio::sync::RwLock::new("transport".into())),
            edge_agent_id: "agent".into(),
            physical_workspace_id: "physical-test".into(),
            identity,
            provider:
                astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
            ask_user_request_tx: Some(ask_tx),
        };
        let auth = gate.auth.clone();
        let execute = gate.request_interaction(&interaction);
        tokio::pin!(execute);
        let prompt = tokio::select! {
            result = &mut execute => panic!("question must arrive before completion: {result:?}"),
            prompt = ask_rx.recv() => prompt.unwrap(),
        };
        assert!(
            prompt
                .prompt
                .context
                .as_deref()
                .unwrap()
                .contains("session")
        );
        *auth.write().await = "rotated-token".into();
        prompt
            .response_tx
            .send(chat_stream::AskUserResponse::Submitted(
                astra_tools::AskUserAnswers {
                    answers: vec![astra_tools::AskUserQuestionAnswer {
                        question: prompt.prompt.questions[0].question.clone(),
                        answers: vec!["A".into()],
                        multi_select: false,
                        annotation: None,
                    }],
                },
            ))
            .unwrap();
        let result = execute.await;
        assert!(matches!(result, Decision::Submitted(_)), "{result:?}");
        drop(server);
    }

    #[tokio::test]
    async fn native_gate_routes_exact_identity_and_never_renews_expired_budget() {
        let server = MockServer::start().await;
        let edge_transport_id =
            std::sync::Arc::new(tokio::sync::RwLock::new("old-transport".into()));
        let mut gate = NativeInvocationInteractionGate {
            api: astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            auth: std::sync::Arc::new(tokio::sync::RwLock::new("fixture-token".into())),
            auth_provider: None,
            edge_transport_id: edge_transport_id.clone(),
            edge_agent_id: "agent".into(),
            physical_workspace_id: "physical-test".into(),
            identity: astra_turn_types::ToolInvocationIdentity::new(
                "account", "session", "run", "chain", "call",
            )
            .unwrap(),
            provider:
                astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
            ask_user_request_tx: None,
        };
        let interaction = astra_turn_types::ProviderInteractionRequest {
            request_id: "native-rpc".into(),
            payload: serde_json::json!({"provider":"codex", "params":{"turnId":"turn"}}),
            timeout_ms: Some(60_000),
            provider_stage_input_id: None,
        };
        let body = astra_thin_client::ToolInteractionRequest {
            identity: gate.identity.clone(),
            edge_agent_id: gate.edge_agent_id.clone(),
            physical_workspace_id: gate.physical_workspace_id.clone(),
            interaction: interaction.clone(),
        };
        Mock::given(method("POST"))
            .and(path("/tools/interactions/request"))
            .and(header("authorization", "Bearer fixture-token"))
            .and(header("X-Astra-Edge-Id", "transport"))
            .and(body_json(serde_json::to_value(body).unwrap()))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                .set_body_string(format!("data: {}\n\n", serde_json::json!({
                    "type":"tool_interaction_response", "response": {
                        "request_id":"native-rpc", "outcome":"submitted", "payload":{"answer":"A"}
                    }
                }))))
            .expect(1)
            .mount(&server)
            .await;
        // A reconnect replaces the server-issued identity while this
        // invocation is still alive. The callback must use the replacement
        // identity, not the transport that admitted the invocation.
        *edge_transport_id.write().await = "transport".into();
        assert!(matches!(
            gate.request_interaction(&interaction).await,
            Decision::Submitted(_)
        ));
        // Advance this invocation's immutable boundary, not the next question's timeout.
        gate.deadline = std::time::Instant::now();
        assert!(matches!(
            gate.request_interaction(&interaction).await,
            Decision::Timeout
        ));
        assert!(matches!(
            gate.request_interaction(&interaction).await,
            Decision::Timeout
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn native_gate_errors_preserve_safe_category_without_private_payload() {
        use astra_thin_client::ThinClientError as E;
        for (error, expected) in [
            (
                E::Api {
                    status: reqwest::StatusCode::UNAUTHORIZED,
                    body: "private-secret".into(),
                },
                "authentication (HTTP 401)",
            ),
            (
                E::Api {
                    status: reqwest::StatusCode::GATEWAY_TIMEOUT,
                    body: "private-secret".into(),
                },
                "deadline (HTTP 504)",
            ),
            (
                E::InvalidProviderInteractionResponse("private-secret".into()),
                "protocol",
            ),
        ] {
            let Decision::Error(message) = native_interaction_error(error) else {
                panic!("expected categorized error")
            };
            assert!(message.contains(expected));
            assert!(!message.contains("private-secret"));
        }
        assert!(matches!(
            native_interaction_error(E::AdmissionDeadlineExpired),
            Decision::Timeout
        ));
    }
}
