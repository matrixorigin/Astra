//! Consumer contract pinned to Matrixflow dev at MOI_CONTRACT_COMMIT below.
//! Run against a disposable MatrixOne instance with ASTRA_TEST_DB_IT=1.
//! The client deliberately uses JSON and its own signer, not Astra request types.
//! Recovery and normal chat share this baseline; updating it requires reviewing
//! the MOI wire contract, not adapting expectations to an Astra API change.

use astra_core::config::{AppSettings, ProviderRequestAuthConfig};
use astra_runtime::server::provider_test_support::openai_response;
use astra_runtime::{build_app, build_server_state};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post as route_post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, MySqlConnection};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;
use uuid::Uuid;

#[path = "moi_fresh_astra/normal_chat.rs"]
mod normal_chat;

const MOI_CONTRACT_COMMIT: &str = "c244138ec330b768e7fb6ff8bbbc97f2d91081aa";
const KEY: &str = "moi-contract-only-provider-signing-key";
const FILE_ID: &str = "11111111-2222-4333-8444-555555555555";
const HISTORY_FACT: &str = "The historical project code is ORCHID-728.";
const ANSWER: &str = "ORCHID-728: historical-file-content";
const FOLLOWUP: &str = "The previous answer was ORCHID-728: historical-file-content";
const SKILL_BODY: &str = "MOI historical-file workflow: report the project code and file content.";

fn file_tool() -> Value {
    json!({"id":"read_file", "name":"read_file", "kind":"workitem", "side_effect_class":"read",
        "description":"Read a historical MOI catalog file", "input_schema":{
        "type":"object", "properties":{"file_id":{"type":"string"}}, "required":["file_id"]
    }})
}

fn file_skill() -> Value {
    json!({"name":"historical-file", "description":"Read the user's historical catalog file"})
}

#[derive(Default)]
struct GatewayState {
    requests: Vec<(String, Value)>,
    errors: Vec<String>,
    model_round: usize,
    bindings: Vec<String>,
    authorization: String,
    reject_model_payment: bool,
}

async fn gateway(
    State(state): State<Arc<Mutex<GatewayState>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut state = state.lock().unwrap();
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(state.authorization.as_str())
    {
        state
            .errors
            .push("capability call did not carry runtime grant".into());
    }
    let method = body["method"].as_str().unwrap_or("model").to_owned();
    state.requests.push((method.clone(), body.clone()));
    let id = body["id"].clone();
    let result = match method.as_str() {
        "tools/list" => json!({"tools":[file_tool()]}),
        "skills/list" => {
            let binding = body["params"]["agent_binding_id"]
                .as_str()
                .unwrap_or_default();
            if !state.bindings.iter().any(|id| id == binding) {
                state
                    .errors
                    .push(format!("unknown skill catalog binding: {binding}"));
            }
            let skills = if state.bindings.get(1).is_some_and(|id| id == binding) {
                vec![file_skill()]
            } else {
                vec![]
            };
            json!({"skills":skills})
        }
        "skills/read" => {
            if body["params"]["agent_binding_id"].as_str()
                != state.bindings.get(1).map(String::as_str)
                || body["params"]["id"] != "historical-file"
            {
                state
                    .errors
                    .push(format!("wrong recovered skill binding: {body}"));
            }
            json!({"skill":{"id":"historical-file","instruction":{"body":SKILL_BODY}}})
        }
        "tools/call" => {
            if body["params"]["name"] != "read_file"
                || body["params"]["arguments"]["file_id"] != FILE_ID
                || body["params"]["call_id"].as_str().is_none_or(str::is_empty)
            {
                state
                    .errors
                    .push(format!("wrong historical file call: {body}"));
            }
            json!({"content":[{"type":"text","text":"historical-file-content"}],"isError":false})
        }
        "model" => {
            if state.reject_model_payment {
                if headers
                    .get("x-moi-model-gateway-error-contract")
                    .and_then(|v| v.to_str().ok())
                    != Some("v1")
                {
                    state
                        .errors
                        .push("missing negotiated MOI gateway error contract".into());
                }
                return (StatusCode::PAYMENT_REQUIRED,
                    [("x-moi-model-gateway-error-contract", "v1")],
                    Json(json!({"error":{"code":"insufficient_credit","retryable":false,
                        "action":"open_billing_overview","message":"private upstream detail must not leak"}})))
                    .into_response();
            }
            let round = state.model_round;
            state.model_round += 1;
            let messages = body["messages"].to_string();
            if body["model"] != "moi-contract-model" {
                state
                    .errors
                    .push(format!("selected model was changed: {}", body["model"]));
            }
            // These assertions inspect the actual model input. A canned answer
            // alone cannot prove that Astra retained history or tool results.
            let message = match round {
                0 => {
                    for layer in ["foundation", "extension"] {
                        if !messages.contains(&format!("MOI {layer} agent")) {
                            state
                                .errors
                                .push(format!("restored {layer} prompt missing"));
                        }
                    }
                    if !messages.contains(HISTORY_FACT) || !messages.contains(FILE_ID) {
                        state.errors.push(
                            "restored history/file reference missing from model input".into(),
                        );
                    }
                    let tools = body["tools"].as_array().expect("model tools");
                    let tool = tools.iter().find_map(|tool| {
                        tool["function"]["name"]
                            .as_str()
                            .filter(|name| *name == "mcp__moi-tools__read_file")
                    });
                    if tool.is_none() {
                        state
                            .errors
                            .push(format!("MOI file tool missing: {}", body["tools"]));
                    }
                    json!({"role":"assistant","content":null,"tool_calls":[{
                        "id":"historical-skill-call","type":"function","function":{
                            "name":"skill","arguments":json!({"skill_name":"historical-file"}).to_string()
                        }
                    }]})
                }
                1 => {
                    if !messages.contains(SKILL_BODY) {
                        state
                            .errors
                            .push("restored binding skill did not reach the model".into());
                    }
                    json!({"role":"assistant","content":null,"tool_calls":[{
                        "id":"historical-file-call","type":"function","function":{
                            "name":"mcp__moi-tools__read_file","arguments":json!({"file_id":FILE_ID}).to_string()
                        }
                    }]})
                }
                2 => {
                    if !messages.contains("historical-file-content") {
                        state.errors.push(format!(
                            "file result did not reach the model: {:?}",
                            body["messages"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .filter(|m| m["role"] == "tool")
                                .collect::<Vec<_>>()
                        ));
                    }
                    json!({"role":"assistant","content":ANSWER})
                }
                3 => {
                    if !messages.contains(ANSWER)
                        || !messages.contains("Repeat the previous answer")
                    {
                        state
                            .errors
                            .push("next turn did not retain the recovered session context".into());
                    }
                    json!({"role":"assistant","content":FOLLOWUP})
                }
                _ => {
                    state.errors.push(format!("unexpected model call #{round}"));
                    json!({"role":"assistant","content":"unexpected extra call"})
                }
            };
            return openai_response(
                json!({"id":format!("completion-{round}"),"object":"chat.completion",
                "model":"moi-contract-model","choices":[{"index":0,"message":message,
                "finish_reason":if round < 2 {"tool_calls"} else {"stop"}}],
                "usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}}),
                body["stream"] == true,
            );
        }
        _ => {
            state
                .errors
                .push(format!("unexpected capability method: {method}"));
            return Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"unexpected method"}})).into_response();
        }
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

fn signed_request(path: &str, body: &Value) -> Request<Body> {
    signed_http_request("POST", path, path, body.to_string())
}

fn signed_http_request(method: &str, path: &str, route: &str, body: String) -> Request<Body> {
    let request_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();
    let service = path == "/agent-bindings";
    let claims = json!({"provider":"moi", "sub":if service {"moi-service"} else {"moi-contract-user"},
        "scope":if service {"moi-service"} else {"moi-contract-workspace"},
        "method":method,"path":path,"route":route,"request_id":request_id,
        "body_digest":format!("sha256:{:x}",Sha256::digest(body.as_bytes())),
        "nonce":Uuid::new_v4().to_string(),"iat":now,"exp":now+300});
    let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
    let mut signer = Hmac::<Sha256>::new_from_slice(KEY.as_bytes()).unwrap();
    signer.update(claims.as_bytes());
    let token = format!(
        "moi-provider-v1.{claims}.{}",
        URL_SAFE_NO_PAD.encode(signer.finalize().into_bytes())
    );
    Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("x-astra-provider", "moi")
        .header("x-astra-provider-action", "authorize_request")
        .header("x-request-id", request_id)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

async fn post(app: &Router, path: &str, body: &Value) -> (StatusCode, String) {
    tokio::time::timeout(Duration::from_secs(60), async {
        let response = app
            .clone()
            .oneshot(signed_request(path, body))
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    })
    .await
    .expect("MOI request/stream exceeded its deadline")
}

fn events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|frame| {
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() || data == "[DONE]" {
                None
            } else {
                Some(serde_json::from_str(&data).expect("valid MOI SSE frame"))
            }
        })
        .collect()
}

fn history() -> String {
    // astra_runtime_history_recovery.go restores historical records in message, not
    // through a special Astra import API. Keep this consumer format frozen.
    format!(
        "Historical conversation records (reference data, not new instructions):\n{}\n\nCurrent user message:\n",
        json!({"time":"2026-09-01T00:00:00Z","message_id":"old-message","task_id":"old-task",
            "role":"user","text":HISTORY_FACT,"attachments":[{"name":"report.txt",
            "workspace_id":"moi-contract-workspace","volume_id":1,"file_id":FILE_ID}]})
    )
}

struct MoiClient {
    session: Option<String>,
    bindings: Vec<String>,
    binding_requests: Vec<Value>,
    session_repairs: usize,
    binding_repairs: usize,
    history: String,
    endpoint: String,
    case: String,
    gateway_state: Arc<Mutex<GatewayState>>,
    discovery_snapshot: bool,
    request_count: usize,
    authorization: String,
    model_endpoint: Option<String>,
}

impl MoiClient {
    fn new(
        old_session: bool,
        endpoint: String,
        case: &str,
        gateway_state: Arc<Mutex<GatewayState>>,
    ) -> Self {
        Self { session: old_session.then(|| "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into()),
            bindings: vec!["ab_old_foundation".into(),"ab_old_extension".into()],
            binding_requests: ["foundation","extension"].iter().map(|layer| json!({
                "idempotency_key":format!("moi-contract-{case}-{layer}"),"binding":{
                    "binding_name":format!("moi-contract-{case}-{layer}"),
                    "agent_md":format!("You are the MOI {layer} agent. Use the catalog file tool."),
                    "metadata":{"source_system":"moi","source_ref":format!("{case}/{layer}"),
                        "source_package_ref":"historical-agent@1.0.0",
                        "prompt_hash":"fixture-prompt-hash",
                        // The pinned MOI emits hashes of its nil capability/policy
                        // values in metadata, but omits both top-level fields.
                        "capability_server_set_hash":format!("sha256:{:x}",Sha256::digest(b"null")),
                        "runtime_policy_hash":format!("sha256:{:x}",Sha256::digest(b"null")),
                        "binding_content_hash":"fixture-binding-hash"},
                    "binding_schema_version":"v1"
                }
            })).collect(),
            session_repairs:0,binding_repairs:0,history:String::new(),endpoint,case:case.into(),gateway_state,
            discovery_snapshot:old_session,request_count:0,authorization:String::new(),model_endpoint:None }
    }

    fn payload(&self, message: &str) -> Value {
        let mut value = json!({"message":format!("{}{message}",self.history),
            "agent_bindings":self.bindings.iter().map(|id| json!({"id":id})).collect::<Vec<_>>(),
            "model_selection":{"offering_id":"moi-contract-offering"},
            "resolved_model_selection":{"offering_id":"moi-contract-offering","model_name":"moi-contract-model"},
            "runtime_auth":{"authorization":self.authorization},
            "capability_descriptors":{
                "model_gateway":{"id":"moi-model-gateway","type":"model_gateway","transport":"http",
                    "endpoint_url":format!("{}/model",self.endpoint),"protocol":"openai_chat_completions","metadata":{},"model_context_window":128000},
                "mcp":{"id":"moi-tools","type":"mcp","transport":"streamable_http","endpoint_url":format!("{}/capabilities",self.endpoint),"protocol":"mcp","metadata":{}},
                "skills":{"id":"moi-skills","type":"skills","transport":"streamable_http","endpoint_url":format!("{}/capabilities",self.endpoint),"protocol":"astra_skills","metadata":{}}
            },"execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"}});
        if let Some(session) = &self.session {
            value["session_id"] = json!(session);
        }
        if let Some(endpoint) = &self.model_endpoint {
            value["capability_descriptors"]["model_gateway"]["endpoint_url"] = json!(endpoint);
        }
        if self.discovery_snapshot {
            value["capability_descriptors"]["discovery_snapshot"] = json!({
                "version":"moi-runtime-capability-discovery-v1", "tools":[file_tool()],
                "skill_catalogs":[
                    {"agent_binding_id":self.bindings[0],"skills":[]},
                    {"agent_binding_id":self.bindings[1],"skills":[file_skill()]}
                ]
            });
        }
        value
    }

    async fn repair_session(&mut self, app: &Router) {
        assert_eq!(
            self.session_repairs, 0,
            "MOI permits only one session repair"
        );
        let identity = json!([
            "moi-contract-workspace",
            "moi-contract-user",
            "historical-agent",
            self.case,
            self.session
        ]);
        let request = json!({"client_session_ref":format!("moi-recovery-{:x}",Sha256::digest(identity.to_string().as_bytes()))});
        let (status, body) = post(app, "/sessions", &request).await;
        assert!(
            status == StatusCode::OK || status == StatusCode::CREATED,
            "{status}: {body}"
        );
        assert!(body.len() <= 4096, "MOI session response limit");
        let response: Value = serde_json::from_str(&body).unwrap();
        let session = response["session_id"]
            .as_str()
            .expect("session_id")
            .to_owned();
        // A lost create response is retried with the same client reference.
        let (status, body) = post(app, "/sessions", &request).await;
        assert!(
            status == StatusCode::OK || status == StatusCode::CREATED,
            "{body}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["session_id"],
            session
        );
        self.session = Some(session);
        self.history = history();
        self.session_repairs += 1;
    }

    async fn repair_bindings(&mut self, app: &Router) {
        assert_eq!(
            self.binding_repairs, 0,
            "MOI permits only one frozen binding-set repair"
        );
        let mut bindings = Vec::new();
        for request in &self.binding_requests {
            let (status, body) = post(app, "/agent-bindings", request).await;
            assert!(status.is_success(), "{status}: {body}");
            let response: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(response["status"], "active");
            assert_eq!(response["binding_name"], request["binding"]["binding_name"]);
            let id = response["agent_binding_id"]
                .as_str()
                .expect("agent_binding_id")
                .to_owned();
            let (status, body) = post(app, "/agent-bindings", request).await;
            assert!(status.is_success(), "{body}");
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap()["agent_binding_id"],
                id
            );
            bindings.push(id);
        }
        self.bindings = bindings;
        self.binding_repairs += 1;
    }

    async fn chat(&mut self, app: &Router, message: &str, expected: &str) -> String {
        // This is the frozen MOI pre-run recovery policy, not a general retry:
        // exact missing-resource codes, matching IDs, one repair of each kind.
        for _ in 0..3 {
            // MOI reissues runtime grants after repair and on subsequent turns.
            // Reusing a stale gateway token must not accidentally pass the test.
            self.request_count += 1;
            self.authorization =
                format!("Bearer moi-contract-{}-{}", self.case, self.request_count);
            {
                let mut gateway = self.gateway_state.lock().unwrap();
                gateway.bindings = self.bindings.clone();
                gateway.authorization = self.authorization.clone();
            }
            let (status, body) = post(app, "/chat/stream", &self.payload(message)).await;
            assert_eq!(status, StatusCode::OK, "MOI expects SSE: {body}");
            let events = events(&body);
            if let Some(error) = events
                .iter()
                .find(|e| e["type"] == "error" || e["type"] == "run_error")
            {
                assert!(
                    events
                        .iter()
                        .all(|e| e["run_id"].as_str().is_none_or(str::is_empty)),
                    "cannot replay an allocated run: {body}"
                );
                match error["error_code"].as_str() {
                    Some("session_not_found") => {
                        assert!(self.session.is_some());
                        assert_eq!(error["session_id"].as_str(), self.session.as_deref());
                        self.repair_session(app).await;
                    }
                    Some("agent_binding_not_found") => {
                        assert!(
                            self.bindings
                                .iter()
                                .any(|id| error["agent_binding_id"] == *id),
                            "missing binding must belong to frozen manifest: {body}"
                        );
                        let session = error["session_id"]
                            .as_str()
                            .expect("binding error must identify the created session");
                        if let Some(existing) = &self.session {
                            assert_eq!(session, existing);
                        }
                        self.session = Some(session.to_owned());
                        self.repair_bindings(app).await;
                    }
                    _ => panic!("not a MOI-recoverable pre-run error: {body}"),
                }
                continue;
            }
            let info = events
                .iter()
                .find(|e| e["type"] == "session_info")
                .expect("session_info");
            let session = info["session_id"]
                .as_str()
                .expect("session_info.session_id");
            if let Some(existing) = &self.session {
                assert_eq!(session, existing);
            }
            self.session = Some(session.to_owned());
            let finished: Vec<_> = events
                .iter()
                .filter(|e| e["type"] == "run_finished")
                .collect();
            assert_eq!(finished.len(), 1, "exactly one terminal event: {body}");
            assert_eq!(finished[0]["status"], "completed", "{body}");
            let run = finished[0]["run_id"].as_str().expect("terminal run_id");
            assert!(!run.is_empty());
            let text: String = events
                .iter()
                .filter(|e| e["type"] == "text_delta")
                .map(|e| e["content"].as_str().expect("text_delta.content"))
                .collect();
            assert_eq!(text, expected, "MOI must see one complete answer: {body}");
            normal_chat::assert_terminal(&events, "completed");
            if expected == ANSWER {
                normal_chat::assert_tool_events(&events);
            }
            self.history.clear();
            return run.to_owned();
        }
        panic!("MOI exhausted its session/binding recovery budget");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; required MOI fresh-Astra compatibility workflow"]
async fn historical_moi_works_with_fresh_astra() {
    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    eprintln!("MOI consumer contract: matrixorigin/matrixflow@{MOI_CONTRACT_COMMIT}");
    let mut settings = AppSettings::from_explicit_env().expect("explicit test settings");
    let database = format!("astra_test_moi_{}", Uuid::new_v4().simple());
    eprintln!("MOI compatibility fixture database: {database}");
    settings.matrixone.database = "mysql".into();
    let mut admin = MySqlConnection::connect(&settings.matrixone.database_url_with_password())
        .await
        .expect("test MatrixOne");
    sqlx::query(&format!("CREATE DATABASE `{database}`"))
        .execute(&mut admin)
        .await
        .unwrap();
    settings.matrixone.database = database.clone();
    settings.provider_request_auth = vec![ProviderRequestAuthConfig {
        provider: "moi".into(),
        auth_type: "hmac".into(),
        key: KEY.into(),
    }];
    settings.external_providers.clear();
    // A strict local fixture owns all external calls. No QA credentials or LLMs.
    let gateway_state = Arc::new(Mutex::new(GatewayState::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    settings.memoria.base_url = endpoint.clone();
    let gateway_app = Router::new()
        .route("/capabilities", route_post(gateway))
        .route("/model", route_post(gateway))
        .with_state(gateway_state.clone());
    let gateway_task = tokio::spawn(async move {
        axum::serve(listener, gateway_app).await.unwrap();
    });
    let state = build_server_state(settings)
        .await
        .expect("fresh Astra bootstrap");
    let app = build_app(state.clone());
    // Each scenario uses a different frozen binding set, absent from this DB.
    // Both start without any Astra-side session corresponding to the MOI data.
    for (old_session, case) in [(true, "old-conversation"), (false, "new-conversation")] {
        *gateway_state.lock().unwrap() = GatewayState::default();
        let mut moi = MoiClient::new(old_session, endpoint.clone(), case, gateway_state.clone());
        let message = if old_session {
            "Read my earlier file and report the project code.".to_owned()
        } else {
            format!("{HISTORY_FACT} Read file {FILE_ID} and report the project code.")
        };
        let first_run = moi.chat(&app, &message, ANSWER).await;
        assert_eq!(moi.session_repairs, usize::from(old_session));
        assert_eq!(moi.binding_repairs, 1);
        let session = moi.session.clone();
        let bindings = moi.bindings.clone();
        let second_run = moi.chat(&app, "Repeat the previous answer", FOLLOWUP).await;
        assert_ne!(first_run, second_run);
        assert_eq!(moi.session, session);
        assert_eq!(moi.bindings, bindings);
        assert_eq!(moi.session_repairs, usize::from(old_session));
        assert_eq!(moi.binding_repairs, 1);
        let calls = gateway_state.lock().unwrap();
        assert!(
            calls.errors.is_empty(),
            "gateway contract violations: {:?}",
            calls.errors
        );
        assert_eq!(calls.model_round, 4, "no duplicate model work");
        assert_eq!(
            calls
                .requests
                .iter()
                .filter(|(method, _)| method == "skills/read")
                .count(),
            1,
            "skill read exactly once"
        );
        let lists = calls
            .requests
            .iter()
            .filter(|(method, _)| method == "skills/list" || method == "tools/list")
            .count();
        if old_session {
            assert_eq!(lists, 0, "frozen discovery must avoid callback discovery");
        } else {
            assert!(lists > 0, "callback discovery must be exercised");
        }
        assert_eq!(
            calls
                .requests
                .iter()
                .filter(|(method, _)| method == "tools/call")
                .count(),
            1,
            "file read exactly once"
        );
        eprintln!(
            "{case}: recovered bindings/session, read Skill/file, completed follow-up; no duplicate calls"
        );
    }
    normal_chat::exercise(&app, &endpoint, gateway_state.clone()).await;
    assert!(
        state.drain_background_runs(Duration::from_secs(10)).await,
        "runs must settle"
    );
    state.close_database_pools().await;
    gateway_task.abort();
    sqlx::query(&format!("DROP DATABASE `{database}`"))
        .execute(&mut admin)
        .await
        .unwrap();
}
