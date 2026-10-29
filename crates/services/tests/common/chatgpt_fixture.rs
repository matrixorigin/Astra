// Shared, non-billable OpenAI protocol fixture for OAuth and inference tests.
use axum::{
    Form, Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

#[derive(Clone)]
struct Grant {
    subject: String,
    nonce: String,
    challenge: String,
    redirect: String,
    plan: bool,
}
pub struct ProviderState {
    base: Mutex<String>,
    key: EncodingKey,
    jwks: Value,
    codes: Mutex<HashMap<String, Grant>>,
    refresh: Mutex<HashMap<String, Grant>>,
    access: Mutex<HashMap<String, Grant>>,
    pub refresh_calls: AtomicUsize,
    pub token_calls: AtomicUsize,
    pub quota: AtomicBool,
    pub fail_refresh: AtomicBool,
    pub requests: Mutex<Vec<Value>>,
    pub responses_calls: AtomicUsize,
    pub hold_responses: AtomicBool,
    pub truncate: AtomicBool,
    pub unterminated: AtomicBool,
    pub discovery_calls: AtomicUsize,
    pub jwks_calls: AtomicUsize,
    pub metadata_delay_ms: AtomicU64,
    pub refresh_delay_ms: AtomicU64,
    pub reject_token: Mutex<Option<String>>,
    pub stale_requests: AtomicUsize,
    pub fresh_seen: AtomicBool,
    pub script: Mutex<Vec<(String, Value)>>,
    pub scripted: AtomicBool,
    pub context_failure: AtomicBool,
    pub stream_quota: AtomicBool,
}
pub struct ProviderFixture {
    pub base: String,
    pub state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for ProviderFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl ProviderFixture {
    pub async fn start() -> Self {
        let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let der = key.to_pkcs1_der().unwrap();
        let jwks = json!({"keys":[{"kty":"RSA","kid":"fixture-key","alg":"RS256","use":"sig",
            "n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())}]});
        let state = Arc::new(ProviderState {
            base: Mutex::new(String::new()),
            key: EncodingKey::from_rsa_der(der.as_bytes()),
            jwks,
            codes: Mutex::new(HashMap::new()),
            refresh: Mutex::new(HashMap::new()),
            access: Mutex::new(HashMap::new()),
            refresh_calls: AtomicUsize::new(0),
            token_calls: AtomicUsize::new(0),
            quota: AtomicBool::new(false),
            fail_refresh: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
            responses_calls: AtomicUsize::new(0),
            hold_responses: AtomicBool::new(false),
            truncate: AtomicBool::new(false),
            unterminated: AtomicBool::new(false),
            discovery_calls: AtomicUsize::new(0),
            jwks_calls: AtomicUsize::new(0),
            metadata_delay_ms: AtomicU64::new(0),
            refresh_delay_ms: AtomicU64::new(0),
            reject_token: Mutex::new(None),
            stale_requests: AtomicUsize::new(0),
            fresh_seen: AtomicBool::new(false),
            script: Mutex::new(Vec::new()),
            scripted: AtomicBool::new(false),
            context_failure: AtomicBool::new(false),
            stream_quota: AtomicBool::new(false),
        });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        *state.base.lock().unwrap() = base.clone();
        let task = spawn_provider(listener, state.clone());
        Self { base, state, task }
    }
    pub async fn stop(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
    pub async fn restart(&mut self) {
        assert!(self.task.is_finished());
        let listener = tokio::net::TcpListener::bind(self.base.trim_start_matches("http://"))
            .await
            .unwrap();
        self.task = spawn_provider(listener, self.state.clone());
    }
    pub fn issue_code(&self, authorization: &Value, subject: &str, plan: bool) -> String {
        let url =
            reqwest::Url::parse(authorization["authorization_url"].as_str().unwrap()).unwrap();
        let fields: HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(fields["code_challenge_method"], "S256");
        assert_eq!(fields["resource"], "https://api.openai.com/v1");
        let code = uuid::Uuid::new_v4().to_string();
        self.state.codes.lock().unwrap().insert(
            code.clone(),
            Grant {
                subject: subject.into(),
                nonce: fields["nonce"].clone(),
                challenge: fields["code_challenge"].clone(),
                redirect: fields["redirect_uri"].clone(),
                plan,
            },
        );
        code
    }
    pub fn id_token(&self, subject: &str, client_id: &str, nonce: &str) -> String {
        sign(&self.state, subject, client_id, nonce)
    }
}
fn spawn_provider(
    listener: tokio::net::TcpListener,
    state: Arc<ProviderState>,
) -> tokio::task::JoinHandle<()> {
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks_handler))
        .route("/token", post(tokens))
        .route("/revoke", post(revoke))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    })
}
fn sign(state: &ProviderState, subject: &str, client_id: &str, nonce: &str) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("fixture-key".into());
    jsonwebtoken::encode(&header,&json!({"iss":*state.base.lock().unwrap(),"sub":subject,"aud":client_id,
        "nonce":nonce,"exp":chrono::Utc::now().timestamp()+3600,"email":format!("{subject}@example.test")}),&state.key).unwrap()
}
async fn discovery(State(state): State<Arc<ProviderState>>) -> Json<Value> {
    state.discovery_calls.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(
        state.metadata_delay_ms.load(Ordering::SeqCst),
    ))
    .await;
    let base = state.base.lock().unwrap().clone();
    Json(
        json!({"issuer":base,"jwks_uri":format!("{base}/jwks"),"revocation_endpoint":format!("{base}/revoke")}),
    )
}
async fn jwks_handler(State(state): State<Arc<ProviderState>>) -> Json<Value> {
    state.jwks_calls.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(
        state.metadata_delay_ms.load(Ordering::SeqCst),
    ))
    .await;
    Json(state.jwks.clone())
}
async fn tokens(
    State(state): State<Arc<ProviderState>>,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    state.token_calls.fetch_add(1, Ordering::SeqCst);
    if form.get("resource").map(String::as_str) != Some("https://api.openai.com/v1")
        || form.get("client_id").map(String::as_str) != Some("oaiapp_fixture")
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_client"})),
        );
    }
    let grant = if form.get("grant_type").map(String::as_str) == Some("authorization_code") {
        let code = form.get("code").cloned().unwrap_or_default();
        let grant = state.codes.lock().unwrap().remove(&code);
        grant.filter(|grant| {
            form.get("code_verifier").is_some_and(|value| {
                URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes())) == grant.challenge
            }) && form.get("redirect_uri") == Some(&grant.redirect)
        })
    } else {
        state.refresh_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(
            state.refresh_delay_ms.load(Ordering::SeqCst),
        ))
        .await;
        if state.fail_refresh.load(Ordering::SeqCst) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"invalid_grant"})),
            );
        }
        let token = form.get("refresh_token").cloned().unwrap_or_default();
        let grant = state.refresh.lock().unwrap().remove(&token);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        grant
    };
    let Some(grant) = grant else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant"})),
        );
    };
    let access = format!("fixture-access-{}", uuid::Uuid::new_v4());
    let refresh = format!("fixture-refresh-{}", uuid::Uuid::new_v4());
    state
        .refresh
        .lock()
        .unwrap()
        .insert(refresh.clone(), grant.clone());
    state
        .access
        .lock()
        .unwrap()
        .insert(access.clone(), grant.clone());
    (
        StatusCode::OK,
        Json(
            json!({"access_token":access,"refresh_token":refresh,"id_token":sign(&state,&grant.subject,"oaiapp_fixture",&grant.nonce),"expires_in":3600,"token_type":"Bearer",
        "scope":if grant.plan {"openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"} else {"openid profile email offline_access"}}),
        ),
    )
}
fn permitted(state: &ProviderState, headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .and_then(|token| state.access.lock().unwrap().get(token).cloned())
        .is_some_and(|grant| grant.plan)
}
async fn models(
    State(state): State<Arc<ProviderState>>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !permitted(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"invalid_token"})),
        );
    }
    (
        StatusCode::OK,
        Json(
            json!({"models":[{"slug":"fixture-model","display_name":"Fixture model","visibility":"list"},
        {"slug":"fixture-second","display_name":"Second fixture","visibility":"list"},{"slug":"hidden","visibility":"hidden"}]}),
        ),
    )
}
async fn revoke(
    State(state): State<Arc<ProviderState>>,
    Form(form): Form<HashMap<String, String>>,
) -> StatusCode {
    if let Some(token) = form.get("token") {
        state.refresh.lock().unwrap().remove(token);
    }
    StatusCode::OK
}
async fn responses(
    State(state): State<Arc<ProviderState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, [(String, String); 1], String) {
    state.responses_calls.fetch_add(1, Ordering::SeqCst);
    if state.hold_responses.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    }
    let content = [("content-type".into(), "text/event-stream".into())];
    let rejected = state.reject_token.lock().unwrap().clone();
    if let Some(rejected) = rejected {
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if auth == Some(rejected.as_str()) {
            let index = state.stale_requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::timeout(std::time::Duration::from_secs(8), async {
                while if index == 0 {
                    state.stale_requests.load(Ordering::SeqCst) < 2
                } else {
                    !state.fresh_seen.load(Ordering::SeqCst)
                } {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            return (StatusCode::UNAUTHORIZED, content, String::new());
        }
        state.fresh_seen.store(true, Ordering::SeqCst);
    }
    if state.context_failure.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            [("content-type".into(), "application/json".into())],
            json!({"error":{"code":"context_length_exceeded","message":"private upstream prose"}})
                .to_string(),
        );
    }
    if state.stream_quota.load(Ordering::SeqCst) {
        let event = json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}});
        return (StatusCode::OK, content, format!("data: {event}\n\n"));
    }
    if !permitted(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, content, String::new());
    }
    if state.quota.load(Ordering::SeqCst) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type".into(), "application/json".into())],
            json!({"error":{"code":"subscription_sharing_usage_limit_exceeded"}}).to_string(),
        );
    }
    if state.truncate.load(Ordering::SeqCst) {
        return (
            StatusCode::OK,
            content,
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".into(),
        );
    }
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    for key in [
        "temperature",
        "max_output_tokens",
        "previous_response_id",
        "messages",
    ] {
        assert!(body.get(key).is_none(), "unexpected {key}");
    }
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["role"] != "system")
    );
    state.requests.lock().unwrap().push(body.clone());
    let continued = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "function_call_output");
    let action = if state.scripted.load(Ordering::SeqCst) && body["tool_choice"] != "none" {
        let mut script = state.script.lock().unwrap();
        if script.is_empty() {
            None
        } else {
            Some(script.remove(0))
        }
    } else {
        None
    };
    let output = if let Some((name, arguments)) = action {
        let index = state.responses_calls.load(Ordering::SeqCst);
        json!([{"type":"reasoning","id":format!("reasoning-{index}"),"encrypted_content":"sk-fixtureciphertextciphertextciphertext","summary":[]},
            {"type":"function_call","id":format!("function-{index}"),"namespace":"astra","name":name,"call_id":format!("call-{index}"),"arguments":arguments.to_string(),"status":"completed"}])
    } else if !state.scripted.load(Ordering::SeqCst)
        && body.get("tools").is_some()
        && body["tool_choice"] != "none"
        && !continued
    {
        let name = body["tools"][0]["tools"][0]["name"].as_str().unwrap();
        json!([{"type":"reasoning","id":"reasoning-1","encrypted_content":"sk-fixtureciphertextciphertextciphertext","summary":[]},
            {"type":"function_call","id":"function-1","namespace":"astra","name":name,"call_id":"call-1","arguments":"{}","status":"completed"}])
    } else {
        json!([{"type":"message","id":"message-1","role":"assistant","phase":"final_answer","status":"completed","content":[{"type":"output_text","text":if continued {"Continued successfully"} else {"OK"},"annotations":[]}]}])
    };
    let event = json!({"type":"response.completed","response":{"id":"response-fixture","status":"completed","model":body["model"],"output":output,
        "usage":{"input_tokens":20,"output_tokens":5,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":2}}}});
    (
        StatusCode::OK,
        content,
        format!(
            "data: {event}{}",
            if state.unterminated.load(Ordering::SeqCst) {
                "\n"
            } else {
                "\n\n"
            }
        ),
    )
}
