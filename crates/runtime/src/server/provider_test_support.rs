//! Strict loopback providers for tests of the real Server execution path.
//! Scripts match actual requests; no host lifecycle or response admission is simulated.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub method: Method,
    pub path: String,
    pub body: Value,
    pub raw_body: Vec<u8>,
}

pub enum ProviderResponse {
    OpenAi(Value),
    Anthropic(Value),
    Bedrock(Value),
    Json {
        status: StatusCode,
        body: Value,
    },
    Stream {
        content_type: &'static str,
        chunks: Vec<Vec<u8>>,
        release_before_chunk: Option<(usize, Arc<Notify>)>,
    },
}

pub struct ProviderScript {
    name: String,
    matches: Arc<dyn Fn(&ProviderRequest) -> bool + Send + Sync>,
    responses: ScriptResponses,
}

enum ScriptResponses {
    Finite {
        remaining: VecDeque<ProviderResponse>,
        allowed_unconsumed: usize,
    },
    OptionalBackgroundJson(Value),
}

impl ProviderScript {
    pub fn new(
        name: impl Into<String>,
        matches: impl Fn(&ProviderRequest) -> bool + Send + Sync + 'static,
        responses: Vec<ProviderResponse>,
    ) -> Self {
        let required = responses.len();
        Self::bounded(name, matches, required, responses)
    }

    /// Background calls can be coalesced by their owner. Keep their matching
    /// and maximum count strict while allowing the declared minimum count.
    pub fn bounded(
        name: impl Into<String>,
        matches: impl Fn(&ProviderRequest) -> bool + Send + Sync + 'static,
        minimum_requests: usize,
        responses: Vec<ProviderResponse>,
    ) -> Self {
        assert!(minimum_requests <= responses.len());
        Self {
            name: name.into(),
            matches: Arc::new(matches),
            responses: ScriptResponses::Finite {
                allowed_unconsumed: responses.len() - minimum_requests,
                remaining: responses.into(),
            },
        }
    }
    /// An out-of-scope background owner may submit any number of snapshots.
    /// Only explicitly matched requests receive this fixed OpenAI response.
    pub fn optional_background_json(
        name: impl Into<String>,
        matches: impl Fn(&ProviderRequest) -> bool + Send + Sync + 'static,
        body: Value,
    ) -> Self {
        Self {
            name: name.into(),
            matches: Arc::new(matches),
            responses: ScriptResponses::OptionalBackgroundJson(body),
        }
    }
}

#[derive(Clone)]
struct GatewayState {
    scripts: Arc<Mutex<Vec<ProviderScript>>>,
    errors: Arc<Mutex<Vec<String>>>,
    requests: Arc<tokio::sync::Mutex<Vec<ProviderRequest>>>,
}

pub struct ProviderGateway {
    pub base_url: String,
    pub requests: Arc<tokio::sync::Mutex<Vec<ProviderRequest>>>,
    state: GatewayState,
    task: tokio::task::JoinHandle<()>,
}

impl ProviderGateway {
    pub async fn start(scripts: Vec<ProviderScript>) -> Self {
        assert!(
            !scripts.is_empty(),
            "provider fixture needs an explicit script"
        );
        let state = GatewayState {
            scripts: Arc::new(Mutex::new(scripts)),
            errors: Arc::new(Mutex::new(Vec::new())),
            requests: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(provider_handler))
            .route("/v1/messages", post(provider_handler))
            .route("/v1/systemone", post(provider_handler))
            .route("/model/{model}/converse", post(provider_handler))
            .route("/model/{model}/converse-stream", post(provider_handler))
            .fallback(unexpected_request)
            .method_not_allowed_fallback(unexpected_request)
            .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind provider fixture");
        let address = listener.local_addr().expect("provider fixture address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve provider fixture");
        });
        Self {
            base_url: format!("http://{address}"),
            requests: state.requests.clone(),
            state,
            task,
        }
    }

    pub fn assert_complete(&self) {
        let errors = self.state.errors.lock().unwrap();
        assert!(errors.is_empty(), "provider fixture errors: {errors:?}");
        let scripts = self.state.scripts.lock().unwrap();
        let pending: Vec<_> = scripts
            .iter()
            .filter_map(|s| match &s.responses {
                ScriptResponses::Finite {
                    remaining,
                    allowed_unconsumed,
                } if remaining.len() > *allowed_unconsumed => {
                    Some((&s.name, remaining.len() - allowed_unconsumed))
                }
                _ => None,
            })
            .collect();
        assert!(
            pending.is_empty(),
            "unconsumed provider responses: {pending:?}"
        );
    }

    pub fn assert_no_fixture_errors(&self) {
        let errors = self.state.errors.lock().unwrap();
        assert!(errors.is_empty(), "provider fixture errors: {errors:?}");
    }
}

impl Drop for ProviderGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn unexpected_request(
    State(state): State<GatewayState>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    state.requests.lock().await.push(ProviderRequest {
        method: method.clone(),
        path: uri.path().into(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        raw_body: body.to_vec(),
    });
    state.errors.lock().unwrap().push(format!(
        "unexpected provider endpoint: {method} {}",
        uri.path()
    ));
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":{"message":"unexpected provider endpoint"}})),
    )
        .into_response()
}

async fn provider_handler(State(state): State<GatewayState>, uri: Uri, body: Bytes) -> Response {
    let Ok(payload) = serde_json::from_slice(&body) else {
        state
            .errors
            .lock()
            .unwrap()
            .push("request was not JSON".into());
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"fixture expected JSON"}})),
        )
            .into_response();
    };
    let request = ProviderRequest {
        method: Method::POST,
        path: uri.path().into(),
        body: payload,
        raw_body: body.to_vec(),
    };
    state.requests.lock().await.push(request.clone());
    let response = {
        let mut scripts = state.scripts.lock().unwrap();
        let matching: Vec<_> = scripts
            .iter()
            .enumerate()
            .filter(|(_, s)| (s.matches)(&request))
            .map(|(i, _)| i)
            .collect();
        match matching.as_slice() {
            [index] => {
                let script = &mut scripts[*index];
                match &mut script.responses {
                    ScriptResponses::Finite { remaining, .. } => remaining
                        .pop_front()
                        .ok_or_else(|| format!("script {} exhausted", script.name)),
                    ScriptResponses::OptionalBackgroundJson(body) => {
                        Ok(ProviderResponse::OpenAi(body.clone()))
                    }
                }
            }
            [] => Err("request matched no script".into()),
            _ => Err(format!(
                "request ambiguously matched {} scripts",
                matching.len()
            )),
        }
    };
    match response {
        Ok(response) => {
            let valid_protocol = match &response {
                ProviderResponse::OpenAi(_) => request.path == "/v1/chat/completions",
                ProviderResponse::Anthropic(_) => request.path == "/v1/messages",
                ProviderResponse::Bedrock(_) => {
                    request.path.starts_with("/model/")
                        && (request.path.ends_with("/converse")
                            || request.path.ends_with("/converse-stream"))
                }
                ProviderResponse::Json { .. } | ProviderResponse::Stream { .. } => true,
            };
            if !valid_protocol {
                state
                    .errors
                    .lock()
                    .unwrap()
                    .push("script response used the wrong provider protocol endpoint".into());
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error":{"message":"fixture provider protocol mismatch"}})),
                )
                    .into_response();
            }
            render_response(response, &request)
        }
        Err(error) => {
            state.errors.lock().unwrap().push(error.clone());
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"message":error}})),
            )
                .into_response()
        }
    }
}

fn render_response(response: ProviderResponse, request: &ProviderRequest) -> Response {
    match response {
        ProviderResponse::Json { status, body } => (status, Json(body)).into_response(),
        ProviderResponse::OpenAi(body) => {
            assert_eq!(
                request.path, "/v1/chat/completions",
                "OpenAI script must use its actual protocol endpoint"
            );
            if request.body["stream"] == true {
                streamed("text/event-stream", openai_events(&body), None)
            } else {
                Json(body).into_response()
            }
        }
        ProviderResponse::Anthropic(body) => {
            assert_eq!(
                request.path, "/v1/messages",
                "Anthropic script must use its actual protocol endpoint"
            );
            if request.body["stream"] == true {
                streamed("text/event-stream", anthropic_events(&body), None)
            } else {
                Json(body).into_response()
            }
        }
        ProviderResponse::Bedrock(body) => {
            assert!(
                request.path.starts_with("/model/"),
                "Bedrock script must use its actual protocol endpoint"
            );
            if request.path.ends_with("/converse-stream") {
                streamed(
                    "application/vnd.amazon.eventstream",
                    bedrock_events(&body),
                    None,
                )
            } else {
                Json(body).into_response()
            }
        }
        ProviderResponse::Stream {
            content_type,
            chunks,
            release_before_chunk,
        } => streamed(content_type, chunks, release_before_chunk),
    }
}

fn streamed(
    content_type: &'static str,
    chunks: Vec<Vec<u8>>,
    gate: Option<(usize, Arc<Notify>)>,
) -> Response {
    let stream = async_stream::stream! {
        for (index, chunk) in chunks.into_iter().enumerate() {
            if let Some((gate_index, release)) = gate.as_ref() && *gate_index == index {
                release.notified().await;
            }
            yield Ok::<Bytes, Infallible>(Bytes::from(chunk));
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(stream))
        .expect("fixture response")
}

fn sse(event: Option<&str>, value: Value) -> Vec<u8> {
    let prefix = event.map_or(String::new(), |event| format!("event: {event}\n"));
    format!("{prefix}data: {value}\n\n").into_bytes()
}

fn openai_events(body: &Value) -> Vec<Vec<u8>> {
    let chunk = |choice: Value, usage: Option<&Value>| {
        let mut event = json!({"object":"chat.completion.chunk","choices":[choice]});
        for field in ["id", "model", "created", "system_fingerprint"] {
            if let Some(value) = body.get(field) {
                event[field] = value.clone();
            }
        }
        if let Some(usage) = usage {
            event["usage"] = usage.clone();
        }
        sse(None, event)
    };
    let message = &body["choices"][0]["message"];
    let mut events = Vec::new();
    for field in ["role", "reasoning_content", "content", "tool_calls"] {
        if let Some(value) = message.get(field) {
            let value = if field == "tool_calls" {
                Value::Array(
                    value
                        .as_array()
                        .expect("explicit OpenAI tool calls")
                        .iter()
                        .enumerate()
                        .map(|(index, call)| {
                            let mut call = call.clone();
                            call["index"] = json!(index);
                            call
                        })
                        .collect(),
                )
            } else {
                value.clone()
            };
            let mut delta = serde_json::Map::new();
            delta.insert(field.to_owned(), value);
            events.push(chunk(json!({"index":0,"delta":delta}), None));
        }
    }
    events.push(chunk(
        json!({"index":0,"delta":{},"finish_reason":body["choices"][0]["finish_reason"]}),
        body.get("usage"),
    ));
    events.push(b"data: [DONE]\n\n".to_vec());
    events
}

fn anthropic_events(body: &Value) -> Vec<Vec<u8>> {
    let mut events = vec![sse(
        Some("message_start"),
        json!({"type":"message_start","message":{
            "id":body["id"],"type":"message","role":"assistant","model":body["model"],
            "content":[],"stop_reason":null,"usage":body["usage"]
        }}),
    )];
    for (index, block) in body["content"]
        .as_array()
        .expect("explicit Anthropic content")
        .iter()
        .enumerate()
    {
        let (start, delta) = match block["type"]
            .as_str()
            .expect("explicit Anthropic block type")
        {
            "text" => (
                json!({"type":"text","text":""}),
                json!({"type":"text_delta","text":block["text"]}),
            ),
            "thinking" => (
                json!({"type":"thinking","thinking":""}),
                json!({"type":"thinking_delta","thinking":block["thinking"]}),
            ),
            "tool_use" => (
                json!({"type":"tool_use","id":block["id"],"name":block["name"],"input":{}}),
                json!({"type":"input_json_delta","partial_json":block["input"].to_string()}),
            ),
            kind => panic!("unsupported explicit Anthropic fixture block {kind}"),
        };
        events.push(sse(
            Some("content_block_start"),
            json!({"type":"content_block_start","index":index,"content_block":start}),
        ));
        events.push(sse(
            Some("content_block_delta"),
            json!({"type":"content_block_delta","index":index,"delta":delta}),
        ));
        events.push(sse(
            Some("content_block_stop"),
            json!({"type":"content_block_stop","index":index}),
        ));
    }
    events.push(sse(Some("message_delta"), json!({"type":"message_delta","delta":{"stop_reason":body["stop_reason"]},"usage":{"output_tokens":body["usage"]["output_tokens"]}})));
    events.push(sse(Some("message_stop"), json!({"type":"message_stop"})));
    events
}

fn bedrock_events(body: &Value) -> Vec<Vec<u8>> {
    let mut events = vec![eventstream_frame(
        "messageStart",
        br#"{"role":"assistant"}"#,
    )];
    for (index, block) in body["output"]["message"]["content"]
        .as_array()
        .expect("explicit Bedrock content")
        .iter()
        .enumerate()
    {
        if let Some(text) = block.get("text") {
            events.push(eventstream_frame(
                "contentBlockDelta",
                &serde_json::to_vec(&json!({"contentBlockIndex":index,"delta":{"text":text}}))
                    .unwrap(),
            ));
        } else if let Some(tool) = block.get("toolUse") {
            events.push(eventstream_frame("contentBlockStart", &serde_json::to_vec(&json!({"contentBlockIndex":index,"start":{"toolUse":{"toolUseId":tool["toolUseId"],"name":tool["name"]}}})).unwrap()));
            events.push(eventstream_frame("contentBlockDelta", &serde_json::to_vec(&json!({"contentBlockIndex":index,"delta":{"toolUse":{"input":tool["input"].to_string()}}})).unwrap()));
        } else {
            panic!("unsupported explicit Bedrock fixture block");
        }
        events.push(eventstream_frame(
            "contentBlockStop",
            &serde_json::to_vec(&json!({"contentBlockIndex":index})).unwrap(),
        ));
    }
    events.push(eventstream_frame(
        "messageStop",
        &serde_json::to_vec(&json!({"stopReason":body["stopReason"]})).unwrap(),
    ));
    events.push(eventstream_frame(
        "metadata",
        &serde_json::to_vec(&json!({"usage":body["usage"],"metrics":{"latencyMs":1}})).unwrap(),
    ));
    events
}

pub(crate) fn eventstream_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
    fn string_header(out: &mut Vec<u8>, name: &str, value: &str) {
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.push(7);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
    }

    let mut headers = Vec::new();
    string_header(&mut headers, ":message-type", "event");
    string_header(&mut headers, ":event-type", event_type);
    let headers_len = headers.len() as u32;
    let total_len = 12 + headers_len + payload.len() as u32 + 4;
    let mut frame = Vec::with_capacity(total_len as usize);
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&headers_len.to_be_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame[..8]).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame
}

/// Reuse the real inference ledger's existing test persistence implementation.
/// This supplies invocation/attempt settlement, never a simulated host loop.
#[derive(Clone, Default)]
pub struct InferenceLedgerFixture {
    pub(crate) persistence: crate::turn::llm::durable::TestInferenceLedgerPersistence,
}

impl InferenceLedgerFixture {
    #[cfg(feature = "e2e-hooks")]
    pub fn admissions(
        &self,
    ) -> Vec<(
        astra_turn_types::InferenceInvocationScope,
        Option<astra_services::InferenceRunAdmissionAuthority>,
    )> {
        self.persistence.admissions()
    }
    /// Await the existing settlement owner without closing provider admission.
    /// Cancellation returns after handoff; logical ledger settlement may follow.
    pub async fn wait_for_settlements(&self, timeout: std::time::Duration) -> bool {
        crate::turn::llm::durable::wait_for_provider_settlement_coordinator(timeout).await
    }
    pub fn assert_quiescent(&self) {
        self.persistence.assert_quiescent();
    }
    pub fn attempt_count(&self) -> usize {
        self.persistence.attempt_count()
    }
    pub fn canonical_transition_hashes(&self) -> Vec<String> {
        self.persistence.canonical_transition_hashes()
    }
}

/// Construct the same explicitly registered Offering used by the real fixture
/// host, including native auxiliary routes without gateway wire overrides.
pub fn admitted_execution(
    gateway: &ProviderGateway,
    provider: &str,
    model: &str,
    cache: Option<astra_services::models::PromptCacheCapabilityData>,
) -> astra_services::AdmittedModelExecution {
    let offering = astra_services::ResolvedModelOffering {
        offering_id: format!("fixture-{provider}"),
        model: astra_services::ResolvedActiveLlmModel {
            price_snapshot: None,
            model_name: model.into(),
            wire_model_name: None,
            api_key: "fixture-key".into(),
            base_url: if provider == "bedrock" {
                gateway.base_url.clone()
            } else {
                format!("{}/v1", gateway.base_url)
            },
            provider: provider.into(),
            tags: Vec::new(),
            request_body_overrides: None,
            fixed_temperature: None,
            thinking_protocol: None,
            prompt_cache_capability: cache,
            thinking_capability: None,
            context_window: Some(128_000),
            max_completion_tokens: Some(4_096),
            request_headers: None,
        },
    };
    astra_services::AdmittedModelExecution::from_offering(offering).unwrap()
}

/// Assemble a real Server host with an explicitly admitted loopback Offering.
/// Semantic-admission tests supply their own policies and provider scripts;
/// these defaults exercise primary request/response execution only.
pub fn server_host_builder(
    gateway: &ProviderGateway,
    ledger: &InferenceLedgerFixture,
    session_id: &str,
    provider: &str,
    model: &str,
    cache: Option<astra_services::models::PromptCacheCapabilityData>,
) -> super::server_loop_host::ServerAgenticLoopHostBuilder {
    let execution = admitted_execution(gateway, provider, model, cache);
    super::server_loop_host::ServerAgenticLoopHostBuilder::new(
        // The fixture supplies its ledger persistence and never connects to a
        // database. Do not depend on dev-defaults or read operator credentials.
        crate::MatrixOneSettings {
            host: "127.0.0.1".into(),
            port: 9,
            user: "provider-fixture".into(),
            password: String::new(),
            database: "provider_fixture".into(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 0,
            db_pool_acquire_timeout_secs: 1,
            db_pool_idle_timeout_secs: 1,
            db_pool_max_lifetime_secs: 1,
        },
        Arc::new(
            crate::FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=")
                .unwrap(),
        ),
        "provider-fixture-user".into(),
        session_id.into(),
    )
    .with_test_inference_ledger(ledger.persistence.clone())
    .with_model_service(Some(Arc::new(OfferingCatalogFixture {
        execution: execution.clone(),
    })))
    .with_admitted_model_execution(Some(execution))
    .with_turn_intent_policy(astra_services::runs::TurnIntentExecutionPolicy::FixedDefault)
    .with_skill_auto_route_policy(astra_services::runs::SkillAutoRouteExecutionPolicy::Disabled)
    .with_static_tool_catalog_admissible(false)
}

pub fn loop_state(
    session_id: &str,
    durable_prefix: Vec<Value>,
    message: &str,
) -> crate::turn::agentic_loop::host::AgenticLoopState {
    let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
    state.current_session_id = Some(session_id.into());
    state.current_run_id = Some(format!("run-{}", uuid::Uuid::new_v4()));
    state.current_run_owner_generation = Some(1);
    state.message = message.into();
    state.user_intent = message.into();
    state.provider_canonical_wal_base =
        Some(astra_turn_types::ProviderCanonicalWalBaseV2::from_messages(&durable_prefix).unwrap());
    state.messages = durable_prefix;
    state
        .messages
        .push(json!({"role":"user", "content":message}));
    state
}

/// Bind the real process-local run ledger and an explicitly selected sandbox.
/// Tools still pass through production admission, invocation and execution.
pub async fn bind_server_workspace(
    state: &mut crate::turn::agentic_loop::host::AgenticLoopState,
    root: &std::path::Path,
) {
    let session = state.current_session_id.clone().expect("fixture session");
    let run = state.current_run_id.clone().expect("fixture run");
    let engine = crate::server::run::engine::RunEngine::new(Arc::new(
        astra_services::runs::InMemoryRunStateStore::new(),
    ));
    let authority = engine
        .start_run(&run, "provider-fixture-user", &session)
        .await
        .expect("admit fixture run");
    state.current_run_owner_generation = Some(authority.owner_generation);
    state.context_manifest_user_id = Some("provider-fixture-user".into());
    state.permission_context = Some(crate::orchestration::PermissionSyncContext::shared_root(
        crate::orchestration::PermissionMode::Auto,
    ));
    state.step_recorder = astra_pipeline::step_recorder::StepRecorder::new(
        "provider-fixture-user",
        &session,
        "provider-fixture-task",
    );
    let ledger =
        crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new_process_local(
            engine,
        )
        .expect("process-local tool ledger");
    let mut executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
        root.into(),
        "provider-fixture-user".into(),
        session,
        None,
        None,
    );
    executor.set_invocation_ledger(ledger);
    executor.set_execution_bindings(
        super::tool_transport::WorkspaceBinding::server_sandbox(root),
        super::tool_transport::ExecutorBinding::server_local(),
    );
    state.runtime_tool_executor = Some(Arc::new(executor));
}

/// A single owner-authorized Offering at the existing ModelService boundary.
/// Every primary request still performs the real catalog revalidation call.
struct OfferingCatalogFixture {
    execution: astra_services::AdmittedModelExecution,
}

#[async_trait::async_trait]
impl astra_services::ModelService for OfferingCatalogFixture {
    async fn admit_model_offering(
        &self,
        user_id: String,
        offering_id: String,
    ) -> Result<astra_services::AdmittedModelExecution, (StatusCode, Json<crate::ErrorResponse>)>
    {
        if user_id != "provider-fixture-user" || offering_id != self.execution.offering_id {
            return Err((
                StatusCode::FORBIDDEN,
                Json(crate::ErrorResponse::new(
                    "fixture Offering is not authorized for this owner",
                )),
            ));
        }
        Ok(self.execution.clone())
    }

    async fn create_model(
        &self,
        user_id: String,
        request: astra_services::ModelCreateRequestData,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<crate::ErrorResponse>)> {
        astra_services::ModelService::create_model(
            &astra_services::models::UnconfiguredModelService,
            user_id,
            request,
        )
        .await
    }
    async fn list_models(
        &self,
        user_id: String,
        is_admin: bool,
    ) -> Result<Vec<astra_services::ModelListItem>, (StatusCode, Json<crate::ErrorResponse>)> {
        astra_services::ModelService::list_models(
            &astra_services::models::UnconfiguredModelService,
            user_id,
            is_admin,
        )
        .await
    }
    async fn get_model(
        &self,
        name: String,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<crate::ErrorResponse>)> {
        astra_services::ModelService::get_model(
            &astra_services::models::UnconfiguredModelService,
            name,
        )
        .await
    }
    async fn resolve_model_offering(
        &self,
        id: String,
    ) -> Result<astra_services::ResolvedModelOffering, (StatusCode, Json<crate::ErrorResponse>)>
    {
        astra_services::ModelService::resolve_model_offering(
            &astra_services::models::UnconfiguredModelService,
            id,
        )
        .await
    }
    async fn update_model(
        &self,
        name: String,
        request: astra_services::ModelUpdateRequestData,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<crate::ErrorResponse>)> {
        astra_services::ModelService::update_model(
            &astra_services::models::UnconfiguredModelService,
            name,
            request,
        )
        .await
    }
    async fn delete_model(
        &self,
        name: String,
    ) -> Result<(), (StatusCode, Json<crate::ErrorResponse>)> {
        astra_services::ModelService::delete_model(
            &astra_services::models::UnconfiguredModelService,
            name,
        )
        .await
    }
    async fn check_model(
        &self,
        name: String,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<crate::ErrorResponse>)> {
        astra_services::ModelService::check_model(
            &astra_services::models::UnconfiguredModelService,
            name,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wrong_endpoint_or_method_is_captured_after_script_completion() {
        for wrong_method in [false, true] {
            let gateway = ProviderGateway::start(vec![ProviderScript::new(
                "expected request",
                |r| r.path == "/v1/chat/completions",
                vec![ProviderResponse::Json {
                    status: StatusCode::OK,
                    body: json!({}),
                }],
            )])
            .await;
            let client = astra_core::net::client_builder_for_target(&gateway.base_url)
                .build()
                .unwrap();
            client
                .post(format!("{}/v1/chat/completions", gateway.base_url))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            gateway.assert_complete();
            let response = if wrong_method {
                client
                    .get(format!("{}/v1/chat/completions", gateway.base_url))
                    .send()
                    .await
                    .unwrap()
            } else {
                client
                    .post(format!("{}/wrong-endpoint", gateway.base_url))
                    .json(&json!({"unexpected":true}))
                    .send()
                    .await
                    .unwrap()
            };
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(gateway.requests.lock().await.len(), 2);
            assert!(
                std::panic::catch_unwind(
                    std::panic::AssertUnwindSafe(|| gateway.assert_complete())
                )
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod response_bounds_tests {
    use super::*;
    #[tokio::test]
    async fn optional_background_keeps_primary_and_matching_strict() {
        fn background() -> ProviderScript {
            ProviderScript::optional_background_json(
                "background",
                |request| {
                    request.path == "/v1/chat/completions"
                        && request.body == json!({"model":"memory", "stream":false})
                },
                json!({"choices":[{"index":0,"message":{"role":"assistant","content":"{}"},"finish_reason":"stop"}]}),
            )
        }
        let gateway = ProviderGateway::start(vec![
            background(),
            ProviderScript::new(
                "primary",
                |request| request.body["model"] == "primary",
                vec![ProviderResponse::OpenAi(json!({"choices":[]}))],
            ),
        ])
        .await;
        let client = astra_core::net::client_builder_for_target(&gateway.base_url)
            .build()
            .unwrap();
        let url = format!("{}/v1/chat/completions", gateway.base_url);
        for _ in 0..5 {
            assert_eq!(
                client
                    .post(&url)
                    .json(&json!({"model":"memory","stream":false}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
        assert_eq!(gateway.requests.lock().await.len(), 5);
        for expected in [StatusCode::OK, StatusCode::BAD_REQUEST] {
            assert_eq!(
                client
                    .post(&url)
                    .json(&json!({"model":"primary"}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                expected
            );
            if expected == StatusCode::OK {
                gateway.assert_complete();
            }
        }
        let optional = ProviderGateway::start(vec![background()]).await;
        optional.assert_complete();
        for body in [
            json!({"model":"wrong","stream":false}),
            json!({"model":"memory","stream":true}),
            json!({"model":"memory","stream":false,"tools":[]}),
        ] {
            assert_eq!(
                client
                    .post(format!("{}/v1/chat/completions", optional.base_url))
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            client
                .post(format!("{}/v1/messages", optional.base_url))
                .json(&json!({"model":"memory","stream":false}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let ambiguous = ProviderGateway::start(vec![background(), background()]).await;
        assert_eq!(
            client
                .post(format!("{}/v1/chat/completions", ambiguous.base_url))
                .json(&json!({"model":"memory","stream":false}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn background_response_bounds_preserve_strict_http_failures() {
        fn response() -> ProviderResponse {
            ProviderResponse::OpenAi(
                json!({"choices":[{"index":0,"message":{"role":"assistant","content":"{}"},"finish_reason":"stop"}]}),
            )
        }
        for attempts in 0..=3 {
            let gateway = ProviderGateway::start(vec![ProviderScript::bounded(
                "one or two owned background requests",
                |request| {
                    request.path == "/v1/chat/completions"
                        && request.body["model"] == "bounded-fixture"
                },
                1,
                vec![response(), response()],
            )])
            .await;
            for index in 0..attempts {
                let result = astra_core::net::client_builder_for_target(&gateway.base_url)
                    .build()
                    .unwrap()
                    .post(format!("{}/v1/chat/completions", gateway.base_url))
                    .json(&json!({"model":"bounded-fixture","stream":false}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    result.status(),
                    if index < 2 {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_REQUEST
                    }
                );
            }
            assert_eq!(gateway.requests.lock().await.len(), attempts);
            let complete = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                gateway.assert_complete()
            }));
            assert_eq!(complete.is_ok(), (1..=2).contains(&attempts));
        }
        let strict = ProviderGateway::start(vec![ProviderScript::new(
            "required primary response",
            |_| true,
            vec![response()],
        )])
        .await;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| strict.assert_complete()))
                .is_err()
        );
    }
}

/// Install the real workspace provider with an explicit fixture capability.
/// Its directory stays alive with the lifecycle and its running clones.
pub fn configure_workspace_provider(
    lifecycle: crate::AgenticRunLifecycleService,
    directory: Arc<tempfile::TempDir>,
    executor_id: &str,
) -> crate::AgenticRunLifecycleService {
    lifecycle.with_fixture_workspace_provider(directory, executor_id)
}

/// Create a canonical record using the fixture's selected real provider.
pub fn provision_workspace_fixture(
    lifecycle: &crate::AgenticRunLifecycleService,
    session_id: &str,
) -> Result<astra_runtime_env::WorkspaceRecord, astra_runtime_env::WorkspaceProvisionError> {
    lifecycle.fixture_server_workspace(session_id)
}

/// Give model-free delegation fixtures the same immutable admission pair as
/// a no-workspace Server run. Callers still own Run creation and status.
pub async fn append_control_plane_contract(
    engine: &crate::server::run::engine::RunEngine,
    user_id: &str,
    session_id: &str,
    run_id: &str,
) -> Result<(), String> {
    let binding = crate::server::tool_transport::ExecutionBindingSnapshot::inferred(
        crate::server::tool_transport::WorkspaceBinding::none(),
        crate::server::tool_transport::ExecutorBinding::server_control_plane(),
    );
    let events = crate::server::run::binding_resolution::binding_snapshot_events(
        run_id,
        session_id,
        &binding,
        &astra_turn_types::StopHookObligations::default(),
    );
    engine
        .append_events_batch(user_id, session_id, run_id, &events)
        .await
}
