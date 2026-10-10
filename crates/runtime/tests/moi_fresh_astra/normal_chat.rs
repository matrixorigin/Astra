//! Normal chat scenarios for the parent module's MOI_CONTRACT_COMMIT baseline.
use super::*;
use astra_runtime::server::provider_test_support::{
    ProviderGateway, ProviderResponse, ProviderScript,
};
use futures_util::StreamExt;
use tokio::sync::Notify;

pub(super) fn assert_terminal(events: &[Value], status: &str) -> String {
    let terminals: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "run_finished")
        .collect();
    assert_eq!(terminals.len(), 1, "one terminal before EOF: {events:?}");
    let terminal = terminals[0];
    assert_eq!(terminal["status"], status, "{events:?}");
    let run = terminal["run_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .expect("terminal run ID");
    let started = events
        .iter()
        .find(|e| e["type"] == "session_info")
        .expect("session_info");
    assert_eq!(started["run_id"], run);
    let last = events.last().unwrap();
    if status == "completed" {
        // MOI uses run_finished as lifecycle truth, then consumes the
        // authoritative final text projection. Neither may replace the other.
        assert_eq!(last["type"], "turn_complete");
        assert_eq!(last["continuation_owner"], "server");
        assert_eq!(last["assistant_text"], text(events));
        assert!(
            !events
                .iter()
                .any(|e| e["type"] == "error" || e["type"] == "run_error")
        );
    } else {
        assert_eq!(
            last["type"], "run_finished",
            "failure/cancel must close the stream"
        );
        assert!(!events.iter().any(|e| e["type"] == "turn_complete"));
    }
    let terminal_index = events
        .iter()
        .position(|e| e["type"] == "run_finished")
        .unwrap();
    assert!(
        events[terminal_index + 1..]
            .iter()
            .all(|e| e["type"] == "turn_complete" || e["type"] == "ping"),
        "no additional work after terminal"
    );
    run.to_owned()
}

pub(super) fn assert_tool_events(events: &[Value]) {
    let events: Vec<_> = events
        .iter()
        .filter(|e| {
            e["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("tool_"))
        })
        .collect();
    for (call, tool, output) in [
        ("historical-skill-call", "skill", SKILL_BODY),
        (
            "historical-file-call",
            "mcp__moi-tools__read_file",
            "historical-file-content",
        ),
    ] {
        let starts: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| e["type"] == "tool_call" && e["tool_call"]["id"] == call)
            .collect();
        let ends: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| e["type"] == "tool_call_end" && e["call_id"] == call)
            .collect();
        assert_eq!(starts.len(), 1, "tool start identity: {events:?}");
        assert_eq!(ends.len(), 1, "tool result identity: {events:?}");
        assert_eq!(starts[0].1["tool_call"]["function"]["name"], tool);
        let arguments: Value = serde_json::from_str(
            starts[0].1["tool_call"]["function"]["arguments"]
                .as_str()
                .expect("tool arguments JSON"),
        )
        .unwrap();
        assert_eq!(
            arguments,
            if tool == "skill" {
                json!({"skill_name":"historical-file"})
            } else {
                json!({"file_id":FILE_ID})
            }
        );
        assert_eq!(ends[0].1["success"], true);
        assert!(starts[0].0 < ends[0].0, "tool result must follow start");
        assert!(
            ends[0].1["result"].to_string().contains(output),
            "MOI-visible tool output: {ends:?}"
        );
    }
}

fn chunk(delta: Value, finish: Value) -> Vec<u8> {
    format!(
        "data: {}\n\n",
        json!({"id":"normal-chat","object":"chat.completion.chunk","model":"moi-contract-model",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
    .into_bytes()
}

fn held_response(release: Arc<Notify>) -> ProviderResponse {
    ProviderResponse::Stream {
        content_type: "text/event-stream",
        chunks: vec![
            chunk(
                json!({"role":"assistant","reasoning_content":"Checking the request."}),
                Value::Null,
            ),
            chunk(json!({"content":"Hello "}), Value::Null),
            chunk(json!({"content":"MOI."}), Value::Null),
            chunk(json!({}), json!("stop")),
            b"data: [DONE]\n\n".to_vec(),
        ],
        // No timer races: the model cannot finish until the consumer has
        // actually received reasoning AND text through Astra's HTTP body.
        release_before_chunk: Some((2, release)),
    }
}

fn answer(text: &str) -> ProviderResponse {
    ProviderResponse::OpenAi(json!({"id":"next-chat","model":"moi-contract-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}}))
}

struct LiveStream {
    stream: axum::body::BodyDataStream,
    parser: sse::MoiSseParser,
}

impl LiveStream {
    async fn open(app: &Router, payload: &Value) -> Self {
        let response = tokio::time::timeout(
            Duration::from_secs(60),
            app.clone().oneshot(signed_request("/chat/stream", payload)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        Self {
            stream: response.into_body().into_data_stream(),
            parser: sse::MoiSseParser::default(),
        }
    }

    async fn read_chunk(&mut self) -> bool {
        if self.parser.done {
            return false;
        }
        let Some(chunk) = tokio::time::timeout(Duration::from_secs(30), self.stream.next())
            .await
            .expect("SSE stalled")
            .map(Result::unwrap)
        else {
            self.parser.finish();
            return false;
        };
        self.parser.feed(&chunk);
        !self.parser.done
    }

    async fn prefix(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !self.parser.events.iter().any(|e| e["type"] == "text_delta") {
                assert!(self.read_chunk().await, "EOF before streaming output");
            }
        })
        .await
        .expect("text buffered until model completion");
        assert!(
            self.parser
                .events
                .iter()
                .any(|e| e["type"] == "reasoning_delta" && e["content"] == "Checking the request."),
            "reasoning stream: {:?}",
            self.parser.events
        );
        assert_eq!(text(&self.parser.events), "Hello ");
        assert!(
            !self
                .parser
                .events
                .iter()
                .any(|e| e["type"] == "run_finished")
        );
        self.parser
            .events
            .iter()
            .find(|e| e["type"] == "session_info")
            .unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn finish(mut self) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(30), async {
            while self.read_chunk().await {}
        })
        .await
        .expect("terminal stream did not close");
        self.parser.events
    }
}

#[tokio::test]
#[should_panic(expected = "one terminal before EOF")]
async fn live_consumer_rejects_early_done() {
    let stream = LiveStream {
        stream: Body::from("data: [DONE]\r\n\r\ndata: {\"type\":\"run_finished\",\"status\":\"completed\"}\r\n\r\n").into_data_stream(),
        parser: sse::MoiSseParser::default(),
    };
    assert_terminal(&stream.finish().await, "completed");
}

fn text(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "text_delta")
        .map(|e| e["content"].as_str().expect("text_delta.content"))
        .collect()
}

async fn cancel(app: &Router, run: &str) -> (StatusCode, Value) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let response = app
            .clone()
            .oneshot(signed_http_request(
                "DELETE",
                &format!("/chat/runs/{run}"),
                "/chat/runs/{run_id}",
                String::new(),
            ))
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    })
    .await
    .expect("cancel deadline")
}

pub(super) async fn exercise(
    app: &Router,
    endpoint: &str,
    gateway_state: Arc<Mutex<GatewayState>>,
) {
    let release = Arc::new(Notify::new());
    let cancel_release = Arc::new(Notify::new());
    let provider = ProviderGateway::start(vec![ProviderScript::new(
        "MOI normal chat/error/cancel",
        |request| {
            request.path == "/v1/chat/completions"
                && request.body["model"] == "moi-contract-model"
                && request.body["stream"] == true
        },
        vec![
            held_response(release.clone()),
            held_response(cancel_release),
            answer("After cancellation."),
            answer("After failure."),
        ],
    )])
    .await;
    *gateway_state.lock().unwrap() = GatewayState::default();
    let mut moi = MoiClient::new(
        false,
        endpoint.into(),
        "normal-protocol",
        gateway_state.clone(),
    );
    moi.repair_session(app).await;
    moi.repair_bindings(app).await;
    moi.history.clear();
    moi.discovery_snapshot = true;
    moi.authorization = "Bearer moi-normal-chat-grant".into();
    {
        let mut gateway = gateway_state.lock().unwrap();
        gateway.authorization = moi.authorization.clone();
        gateway.bindings = moi.bindings.clone();
    }
    moi.model_endpoint = Some(format!("{}/v1/chat/completions", provider.base_url));

    let mut live = LiveStream::open(app, &moi.payload("Say hello.")).await;
    let run = live.prefix().await;
    release.notify_one();
    let complete = live.finish().await;
    assert_eq!(assert_terminal(&complete, "completed"), run);
    assert_eq!(text(&complete), "Hello MOI.");
    let (status, body) = cancel(app, &run).await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["run_id"], run);
    assert_eq!(
        body["status"], "completed",
        "cancel must not rewrite a completed run"
    );

    let mut live = LiveStream::open(app, &moi.payload("Wait for cancellation.")).await;
    let run = live.prefix().await;
    // A second turn must not interrupt or replace an already active run.
    let (status, body) = post(app, "/chat/stream", &moi.payload("Conflicting request.")).await;
    // /chat/stream carries pre-run rejections in SSE even though the
    // underlying admission failure is a conflict; MOI consumes its fields.
    assert_eq!(status, StatusCode::OK, "{body}");
    let rejection = events(&body);
    assert_eq!(
        rejection.len(),
        1,
        "rejected request must not allocate another run"
    );
    let rejected = &rejection[0];
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["code"], "CONFLICT");
    assert_eq!(rejected["session_id"].as_str(), moi.session.as_deref());
    assert_eq!(rejected["metadata"]["admission_state"], "rejected");
    assert_eq!(rejected["retryable"], false);
    assert!(rejected.get("run_id").is_none());
    assert_eq!(
        rejected["error_code"], "session_execution_slot_occupied",
        "{body}"
    );
    let (status, body) = cancel(app, &run).await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["run_id"], run);
    // The pinned MOI accepts durable cancellation intent, then reconciles the
    // terminal state. Acceptance alone must not prove execution has stopped.
    assert!(
        matches!(
            body["status"].as_str(),
            Some("cancelled" | "cancellation_requested")
        ),
        "MOI cancellation response: {body}"
    );
    let cancelled = live.finish().await;
    assert_eq!(assert_terminal(&cancelled, "cancelled"), run);
    assert_eq!(
        text(&cancelled),
        "Hello ",
        "cancelled output must not continue"
    );
    // New DBs no longer know historical run IDs. MOI treats only this exact
    // structured 404 as an already-cancelled run, not arbitrary HTTP failures.
    let (status, body) = cancel(app, "11111111-aaaa-4bbb-8ccc-eeeeeeeeeeee").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error_code"], "run_not_found");
    moi.chat(app, "Continue after cancellation.", "After cancellation.")
        .await;

    moi.model_endpoint = None;
    gateway_state.lock().unwrap().reject_model_payment = true;
    let (status, body) = post(
        app,
        "/chat/stream",
        &moi.payload("Trigger billing rejection."),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "accepted run reports failure through SSE"
    );
    let failed = events(&body);
    let failed_run = assert_terminal(&failed, "failed");
    let errors: Vec<_> = failed.iter().filter(|e| e["type"] == "run_error").collect();
    assert_eq!(errors.len(), 1, "one public failure: {body}");
    assert_eq!(errors[0]["run_id"], failed_run);
    assert_eq!(errors[0]["error_code"], "insufficient_credit", "{errors:?}");
    assert_eq!(errors[0]["http_status"], 402);
    assert_eq!(errors[0]["retryable"], false);
    assert_eq!(errors[0]["action"], "open_billing_overview");
    assert!(errors[0]["message"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(!body.contains("private upstream detail"));
    assert!(
        text(&failed).is_empty(),
        "no successful answer on rejection"
    );
    gateway_state.lock().unwrap().reject_model_payment = false;
    moi.model_endpoint = Some(format!("{}/v1/chat/completions", provider.base_url));
    moi.chat(app, "Continue after failure.", "After failure.")
        .await;
    {
        let gateway = gateway_state.lock().unwrap();
        assert!(gateway.errors.is_empty(), "{:?}", gateway.errors);
        assert_eq!(
            gateway
                .requests
                .iter()
                .filter(|(method, _)| method == "model")
                .count(),
            1,
            "non-retryable gateway failure must not repeat inference"
        );
    }
    provider.assert_complete();
    eprintln!(
        "normal protocol: incremental reasoning/text, tool events, terminal success/failure, conflict, cancel and session reuse"
    );
}
