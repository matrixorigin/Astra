//! Real Astra router/DB with a deterministic upstream fresh-auth attester.
//! The attester's actual email/one-time-consume contract is tested in the website.
use astra_core::{AppSettings, MatrixOneSettings, MemoriaSettings};
use astra_runtime::{build_app, build_server_state};
use astra_thin_client::device_proof::{DeviceProofPurpose, device_challenge_proof};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tower::ServiceExt;
#[path = "../../services/tests/common/isolated_database.rs"]
mod isolated_database;

async fn request(
    app: Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let res = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    if path == "/auth/reauthenticate" && status.is_success() {
        assert_eq!(res.headers()["cache-control"], "no-store");
    }
    let bytes = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"non_json":String::from_utf8_lossy(&bytes)})),
    )
}
fn evidence(proofs: &Mutex<HashMap<String, Value>>, owner: &str, purpose: &str) -> String {
    let proof = format!(
        "msu_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let now = chrono::Utc::now().timestamp();
    proofs.lock().unwrap().insert(proof.clone(),json!({"subject":owner,"key_id":"generation-1","purpose":purpose,"authenticated_at":now,"expires_at":now+120}));
    proof
}
async fn reauth(app: &Router, token: &str, proof: &str, purpose: &str) -> (StatusCode, Value) {
    request(
        app.clone(),
        "POST",
        "/auth/reauthenticate",
        token,
        json!({"memoria_proof":proof,"purpose":purpose}),
    )
    .await
}

#[tokio::test]
#[ignore = "requires explicitly isolated ASTRA_TEST_DATABASE and ASTRA_TEST_DB_IT=1"]
async fn memoria_step_up_authorizes_real_device_and_takeover_routes_without_passwords() {
    let db = MatrixOneSettings::from_env();
    isolated_database::require_isolated_database(&db.database);
    let owner = uuid::Uuid::new_v4().to_string();
    let identity_owner = owner.clone();
    let active = Arc::new(AtomicBool::new(true));
    let identity_active = active.clone();
    let proofs = Arc::new(Mutex::new(HashMap::<String, Value>::new()));
    let issued = proofs.clone();
    let pause_attestation = Arc::new(AtomicBool::new(false));
    let attestation_entered = Arc::new(tokio::sync::Notify::new());
    let attestation_release = Arc::new(tokio::sync::Notify::new());
    let pause = pause_attestation.clone();
    let entered = attestation_entered.clone();
    let release = attestation_release.clone();
    let upstream = Router::new()
        .route("/auth/whoami",get(move |headers: axum::http::HeaderMap| {
            let owner = identity_owner.clone(); let active = identity_active.load(Ordering::SeqCst);
            async move {
                let valid = active && headers.get("authorization").is_some_and(|v| v == "Bearer connection-key");
                (if valid {StatusCode::OK} else {StatusCode::UNAUTHORIZED},Json(json!({"user_id":owner,"key_id":"generation-1","is_active":true,"is_master":false,"scope":{"type":"personal","id":owner},"api_version":"1","capabilities":["api_key_scopes","memory_filters_v1"],"granted_scopes":["identity:read"]})))
            }
        }))
        .route("/api/auth/astra/reauthentication/consume",post(move |Json(body):Json<Value>| {
            let result = issued.lock().unwrap().remove(body["proof"].as_str().unwrap_or(""));
            let paused = pause.swap(false, Ordering::SeqCst);
            let entered = entered.clone();
            let release = release.clone();
            async move {
                if paused {
                    entered.notify_one();
                    release.notified().await;
                }
                match result { Some(value) => (StatusCode::OK,Json(value)), None => (StatusCode::UNAUTHORIZED,Json(json!({}))) }
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let upstream_task = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let mut settings = AppSettings::from_map(&HashMap::from([
        ("MATRIXONE_PASSWORD".into(), db.password.clone()),
        (
            "ASTRA_JWT_SECRET".into(),
            "stepup-test-only-jwt-32-bytes-secret".into(),
        ),
        (
            "ASTRA_RUNTIME_ROOT_SECRET".into(),
            "stepup-test-only-root-secret".into(),
        ),
        (
            "ASTRA_TOKEN_ENCRYPTION_KEY".into(),
            "stepup-test-only-token-key".into(),
        ),
        ("ASTRA_AUTO_CREATE_DATABASE".into(), "1".into()),
    ]))
    .unwrap();
    settings.matrixone = db;
    settings.memoria = MemoriaSettings {
        base_url: base.clone(),
        web_url: Some(base.clone()),
        issuer: Some("https://stepup-issuer.test".into()),
        master_key: None,
        self_hosted_master_access: false,
        legacy_issuer: None,
    };
    let state = build_server_state(settings).await.unwrap();
    let pool = state.shared_pool.as_ref().unwrap().clone();
    let app = build_app(state);
    let (status, login) = request(
        app.clone(),
        "POST",
        "/auth/memoria",
        "",
        json!({"connection_key":"connection-key"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    let token = login["access_token"].as_str().unwrap();
    let user = login["user_id"].as_str().unwrap();
    let (status, options) =
        request(app.clone(), "GET", "/auth/reauthenticate", token, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        options["verification_url"],
        format!("{base}/astra/reauthenticate")
    );
    for old in ["connection-key", token, ""] {
        assert_eq!(
            reauth(&app, token, old, "device_trust").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    for (subject, purpose) in [
        ("another-owner", "device_trust"),
        (owner.as_str(), "device_reenroll"),
    ] {
        let proof = evidence(&proofs, subject, purpose);
        assert_eq!(
            reauth(&app, token, &proof, "device_trust").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let expired = evidence(&proofs, &owner, "device_trust");
    proofs.lock().unwrap().get_mut(&expired).unwrap()["authenticated_at"] =
        json!(chrono::Utc::now().timestamp() - 180);
    assert_eq!(
        reauth(&app, token, &expired, "device_trust").await.0,
        StatusCode::UNAUTHORIZED
    );
    let session = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (session_id,user_id) VALUES (?,?)")
        .bind(&session)
        .bind(user)
        .execute(pool.get())
        .await
        .unwrap();
    let path = format!("/sessions/{session}/device");
    let (status, enrolled) = request(
        app.clone(),
        "POST",
        &format!("{path}/enroll"),
        token,
        json!({"device_id":"laptop","device_fingerprint":"fp-1"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{enrolled}");
    let (status, challenge) = request(
        app.clone(),
        "POST",
        &format!("{path}/challenge"),
        token,
        json!({"device_id":"laptop","device_fingerprint":"fp-1","purpose":"trust"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    let device_proof = device_challenge_proof(
        enrolled["device_key"].as_str().unwrap(),
        DeviceProofPurpose::Trust,
        user,
        &session,
        "laptop",
        "fp-1",
        challenge["challenge_id"].as_str().unwrap(),
        challenge["challenge"].as_str().unwrap(),
    );
    let fresh = evidence(&proofs, &owner, "device_trust");
    let (status, trust) = reauth(&app, token, &fresh, "device_trust").await;
    assert_eq!(status, StatusCode::OK, "{trust}");
    assert_eq!(
        reauth(&app, token, &fresh, "device_trust").await.0,
        StatusCode::UNAUTHORIZED,
        "upstream proof replay"
    );
    let (status,trusted) = request(app.clone(),"POST",&format!("{path}/trust"),token,json!({"device_id":"laptop","device_fingerprint":"fp-1","challenge_id":challenge["challenge_id"],"device_proof":device_proof,"reauthentication_proof":trust["proof"]})).await;
    assert_eq!(status, StatusCode::OK, "{trusted}");
    let fresh = evidence(&proofs, &owner, "device_reenroll");
    let (status, reenroll) = reauth(&app, token, &fresh, "device_reenroll").await;
    assert_eq!(status, StatusCode::OK, "{reenroll}");
    let body = json!({"device_id":"laptop","device_fingerprint":"fp-2","reauthentication_proof":reenroll["proof"]});
    let (status, result) = request(
        app.clone(),
        "POST",
        &format!("{path}/enroll"),
        token,
        body.clone(),
    )
    .await;
    assert!(status.is_success(), "{status}: {result}");
    assert_eq!(
        request(app.clone(), "POST", &format!("{path}/enroll"), token, body)
            .await
            .0,
        StatusCode::FORBIDDEN,
        "Astra proof replay"
    );

    let (status, attachment) = request(
        app.clone(),
        "POST",
        &format!("/sessions/{session}/attachments"),
        token,
        json!({"idempotency_key":uuid::Uuid::new_v4().to_string(),"placement":"server"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{attachment}");
    let fresh = evidence(&proofs, &owner, "session_forced_takeover");
    let (status, takeover) = reauth(&app, token, &fresh, "session_forced_takeover").await;
    assert_eq!(status, StatusCode::OK, "{takeover}");
    let attachment_id = attachment["attachment"]["attachment_id"]
        .as_str()
        .expect("attachment response");
    let mut handoff = json!({"idempotency_key":uuid::Uuid::new_v4().to_string(),"mode":"forced","to_attachment_id":attachment_id,"from_placement":"cli","reason":"verified recovery","reauthentication_proof":takeover["proof"]});
    let (status, result) = request(
        app.clone(),
        "POST",
        &format!("/sessions/{session}/handoffs"),
        token,
        handoff.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    handoff["idempotency_key"] = json!(uuid::Uuid::new_v4().to_string());
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &format!("/sessions/{session}/handoffs"),
            token,
            handoff
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let fresh = evidence(&proofs, &owner, "device_reenroll");
    let (_, pending) = reauth(&app, token, &fresh, "device_reenroll").await;
    active.store(false, Ordering::SeqCst);
    assert_eq!(
        reauth(
            &app,
            token,
            &evidence(&proofs, &owner, "device_trust"),
            "device_trust"
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(request(app.clone(),"POST",&format!("{path}/enroll"),token,json!({"device_id":"laptop","device_fingerprint":"fp-3","reauthentication_proof":pending["proof"]})).await.0,StatusCode::UNAUTHORIZED,"revocation after proof issuance");
    active.store(true, Ordering::SeqCst);

    // A routine login must not invalidate an uninterrupted connection's proof.
    let (status, again) = request(
        app.clone(),
        "POST",
        "/auth/memoria",
        "",
        json!({"connection_key":"connection-key"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["user_id"], user);
    let (status, result) = request(app.clone(),"POST",&format!("{path}/enroll"),token,json!({"device_id":"laptop","device_fingerprint":"fp-3","reauthentication_proof":pending["proof"]})).await;
    assert!(status.is_success(), "routine login: {status}: {result}");

    let (status, pending) = reauth(
        &app,
        token,
        &evidence(&proofs, &owner, "device_reenroll"),
        "device_reenroll",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, pending_takeover) = reauth(
        &app,
        token,
        &evidence(&proofs, &owner, "session_forced_takeover"),
        "session_forced_takeover",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Pause the upstream response after the first binding read. Reconnecting
    // the exact same upstream key must still change Astra's local lifecycle.
    pause_attestation.store(true, Ordering::SeqCst);
    let racing_app = app.clone();
    let racing_token = token.to_owned();
    let racing_proof = evidence(&proofs, &owner, "device_reenroll");
    let racing = tokio::spawn(async move {
        reauth(&racing_app, &racing_token, &racing_proof, "device_reenroll").await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        attestation_entered.notified(),
    )
    .await
    .expect("attester reached");
    assert!(
        request(app.clone(), "DELETE", "/auth/memoria", token, json!({}))
            .await
            .0
            .is_success()
    );
    let (status, relogin) = request(
        app.clone(),
        "POST",
        "/auth/memoria",
        "",
        json!({"connection_key":"connection-key"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{relogin}");
    assert_eq!(relogin["user_id"], user);
    let token = relogin["access_token"].as_str().unwrap();
    attestation_release.notify_one();
    let (status, result) = racing.await.unwrap();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "in-flight proof crossed disconnect: {result}"
    );
    let (status, result) = request(app.clone(),"POST",&format!("{path}/enroll"),token,json!({"device_id":"laptop","device_fingerprint":"fp-4","reauthentication_proof":pending["proof"]})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "old proof revived: {result}");
    let (status, result) = request(app.clone(),"POST",&format!("/sessions/{session}/handoffs"),token,json!({"idempotency_key":uuid::Uuid::new_v4().to_string(),"mode":"forced","to_attachment_id":attachment_id,"from_placement":"cli","reason":"recovery","reauthentication_proof":pending_takeover["proof"]})).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "old takeover proof revived: {result}"
    );
    let (status, fresh) = reauth(
        &app,
        token,
        &evidence(&proofs, &owner, "device_reenroll"),
        "device_reenroll",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fresh}");
    let (status, result) = request(app.clone(),"POST",&format!("{path}/enroll"),token,json!({"device_id":"laptop","device_fingerprint":"fp-4","reauthentication_proof":fresh["proof"]})).await;
    assert!(
        status.is_success(),
        "fresh proof after reconnect: {status}: {result}"
    );
    upstream_task.abort();
}
