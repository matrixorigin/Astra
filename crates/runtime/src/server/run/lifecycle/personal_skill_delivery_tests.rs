//! Exercise published adoption through real HTTP, database, and provider boundaries.
use super::*;
use axum::{
    body::{self, Body},
    http::Request,
    response::IntoResponse,
    routing::post,
};
use tower::ServiceExt;

async fn post_body(app: &axum::Router, owner: &str, path: &str, payload: Value) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("x-user-id", owner)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(status.is_success(), "{path}: {status}: {text}");
    text
}

async fn post_json(app: &axum::Router, owner: &str, path: &str, payload: Value) -> Value {
    serde_json::from_str(&post_body(app, owner, path, payload).await).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and MatrixOne"]
async fn adopted_skill_reaches_primary_provider_in_both_chat_entries() {
    let pool = setup_lifecycle_run_db_it().await;
    type Captures = Arc<TokioMutex<Vec<Value>>>;
    async fn respond(
        axum::extract::State(captures): axum::extract::State<Captures>,
        Json(request): Json<Value>,
    ) -> axum::response::Response {
        captures.lock().await.push(request.clone());
        if request["stream"] == true {
            let chunk =
                json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"Hello."}}]});
            let terminal = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}});
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {chunk}\n\ndata: {terminal}\n\ndata: [DONE]\n\n"),
            )
                .into_response()
        } else {
            Json(json!({"choices":[{"message":{"role":"assistant","content":"Hello."},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}})).into_response()
        }
    }
    let captures = Arc::new(TokioMutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let router = axum::Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(captures.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let _llm = TerminalTestLlm {
        base_url: base_url.clone(),
        requests: Arc::new(AtomicUsize::new(0)),
        server,
    };
    let offering = format!("adopted-offering-{}", Uuid::new_v4());
    let model = format!("adopted-model-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO infra_llm_models (model_id, model_name, provider, api_key_encrypted, base_url, is_active, context_window, input_modalities, output_modalities, supported_parameters, pricing, tags, quirks) VALUES (?, ?, 'openai', ?, ?, 1, 128000, ?, ?, ?, ?, ?, ?)")
        .bind(&offering).bind(&model).bind(test_encryptor().encrypt("test-key").unwrap()).bind(&base_url)
        .bind("[\"text\"]").bind("[\"text\"]").bind("[]").bind("{}").bind("[]").bind("{}")
        .execute(pool.get()).await.unwrap();
    let models = Arc::new(
        astra_services::DatabaseModelService::new(pool.settings().clone(), test_encryptor())
            .with_pool(pool.clone()),
    );
    let lifecycle = Arc::new(
        db_backed_test_service(&pool, "adopted-delivery-pod").with_model_service(models.clone()),
    );
    let owner = format!("adopted-owner-{}", Uuid::new_v4());
    let app = crate::build_app(
        crate::AppState::new(crate::ServiceInfo::default(), Arc::new(EvalHttpHealth))
            .with_fernet_encryptor(test_encryptor().as_ref().clone())
            .with_auth_service(Arc::new(EvalHttpAuth))
            .with_session_service(Arc::new(
                astra_services::DatabaseSessionService::new(pool.settings().clone())
                    .with_pool(pool.clone()),
            ))
            .with_shared_pool(pool.clone())
            .with_model_service(models)
            .with_run_lifecycle_service(lifecycle.clone()),
    );
    let name = "personal-delivery";
    let mut versions = Vec::new();
    let bodies = [
        format!(
            "V1_START\n{}\nV1_END",
            "Use concrete examples.\n".repeat(1200)
        ),
        "V2_START\nAnswer briefly and cite evidence.\nV2_END".to_string(),
        format!(
            "OVERSIZED_START\n{}\nOVERSIZED_END",
            "不可截断的完整指令。".repeat(60000)
        ),
    ];
    for (index, content) in bodies.iter().enumerate() {
        versions.push(post_json(&app, &owner, &format!("/skills/user/{name}/versions"), json!({
            "version":format!("v{index}"), "manifest_json":{"name":name}, "content_markdown":content,"status":"published"
        })).await);
    }
    let mut created = Vec::new();
    for path in ["/chat", "/chat/stream"] {
        let session = post_json(
            &app,
            &owner,
            "/sessions",
            json!({"title":"adopted delivery"}),
        )
        .await;
        let session_id = session["session_id"].as_str().unwrap();
        let mut expected = Value::Null;
        // Fresh adoption, ordinary continuation, replacement, rollback, then oversized rejection.
        for (turn, index) in [0, 0, 1, 0, 2].into_iter().enumerate() {
            if turn != 1 {
                post_json(&app, &owner, &format!("/skills/user/{name}/activate"), json!({
                    "session_id":session_id,"version_id":versions[index]["version_id"],"expected_active_version_id":expected
                })).await;
                expected = versions[index]["version_id"].clone();
            }
            let before = captures.lock().await.len();
            let response = post_body(&app, &owner, path, json!({"message":"Say hello.","session_id":session_id,"model_selection":{"offering_id":offering}})).await;
            let run_id = if path == "/chat" {
                serde_json::from_str::<Value>(&response).unwrap()["run_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            } else {
                response
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                    .find_map(|event| {
                        event["run_id"]
                            .as_str()
                            .or_else(|| event["data"]["run_id"].as_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| panic!("stream has no run identity: {response}"))
            };
            let durable = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let run = lifecycle
                        .run_engine
                        .load_run(&owner, &run_id)
                        .await
                        .unwrap()
                        .unwrap();
                    if !matches!(run.status.as_str(), "pending" | "running" | "queued") {
                        break run;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("ordinary chat must settle");
            let captured = captures.lock().await;
            if index == 2 {
                assert_eq!(durable.status, "paused", "{durable:?}");
                assert_eq!(
                    captured.len(),
                    before,
                    "oversized instructions must fail before provider execution"
                );
                assert!(
                    durable
                        .events
                        .iter()
                        .any(|event| event["event_type"] == "run_interrupted"
                            && event["data"]["kind"] == "context_overflow"),
                    "{durable:?}"
                );
            } else {
                assert_eq!(
                    durable.status, "completed",
                    "{durable:?}; response={response}"
                );
                assert!(
                    captured.len() > before,
                    "must observe actual primary provider request"
                );
                for request in &captured[before..] {
                    let text = request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|message| {
                            if let Some(text) = message["content"].as_str() {
                                text.to_string()
                            } else {
                                message["content"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .filter_map(|block| block["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    assert!(
                        text.contains(&bodies[index]),
                        "full adopted body absent from provider request"
                    );
                    assert!(text.contains(versions[index]["version_id"].as_str().unwrap()));
                    assert!(text.contains(versions[index]["content_hash"].as_str().unwrap()));
                    assert_eq!(
                        text.matches(if index == 0 { "V1_START" } else { "V2_START" })
                            .count(),
                        1
                    );
                    assert!(
                        !text.contains(if index == 0 { "V2_START" } else { "V1_START" }),
                        "replaced instructions leaked through history"
                    );
                }
            }
            drop(captured);
            created.push(run_id);
        }
    }
    for run in created {
        cleanup_lifecycle_run_fixture(&pool, &owner, &run).await;
    }
    for table in [
        "session_state_items",
        "session_state_item_events",
        "agent_events",
        "agent_sessions",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id = ?"))
            .bind(&owner)
            .execute(pool.get())
            .await
            .unwrap();
    }
    for table in ["user_skill_versions", "user_skill_sources"] {
        // All fixtures use a unique owner; production data is never selected.
        sqlx::query(&format!("DELETE FROM {table} WHERE owner_user_id = ?"))
            .bind(&owner)
            .execute(pool.get())
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM infra_llm_models WHERE model_id = ?")
        .bind(&offering)
        .execute(pool.get())
        .await
        .unwrap();
}
