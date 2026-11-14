use super::*;
use axum::{Json, Router, extract::State, routing::post};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated MatrixOne and ASTRA_TEST_DB_IT=1"]
async fn invalid_work_graph_is_corrected_through_native_run() {
    exercise_work_graph_input_recovery(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated MatrixOne and ASTRA_TEST_DB_IT=1"]
async fn valid_work_graph_native_control() {
    exercise_work_graph_input_recovery(false).await;
}

async fn exercise_work_graph_input_recovery(invalid_first: bool) {
    #[derive(Clone)]
    struct Probe {
        pool: SharedPool,
        owner: String,
        invalid_first: bool,
        calls: Arc<AtomicUsize>,
        observations: Arc<TokioMutex<Vec<Value>>>,
    }
    async fn respond(
        State(probe): State<Probe>,
        Json(request): Json<Value>,
    ) -> axum::response::Response {
        let ordinal = probe
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM works WHERE owner_id = ?")
            .bind(&probe.owner)
            .fetch_one(probe.pool.get())
            .await
            .unwrap();
        let operations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_establishment_operations WHERE owner_id = ?",
        )
        .bind(&probe.owner)
        .fetch_one(probe.pool.get())
        .await
        .unwrap();
        let attempt: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT status, outcome FROM work_item_attempts WHERE owner_id = ?")
                .bind(&probe.owner)
                .fetch_optional(probe.pool.get())
                .await
                .unwrap();
        let messages = request["messages"].as_array().expect("provider messages");
        let feedback: Vec<_> = messages
            .iter()
            .filter(|row| row["role"] == "tool")
            .cloned()
            .collect();
        let tools: Vec<_> = request["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .map(str::to_string)
            .collect();
        probe.observations.lock().await.push(json!({
            "round":ordinal, "works":works, "operations":operations,
            "attempt":attempt, "tools":tools, "feedback":feedback
        }));
        let tool = |name: &str, id: &str, args: Value| {
            json!({
                "role":"assistant","content":null,"tool_calls":[{
                    "id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}
                }]
            })
        };
        let message = if works == 0 {
            let invalid = probe.invalid_first && ordinal == 1;
            tool(
                "start_work",
                if invalid {
                    "invalid-work-input"
                } else {
                    "corrected-work-input"
                },
                json!({
                    "goal":"Observe the live runtime and deliver one task.",
                    "activation":"start",
                    "tasks":[{
                        "objective":"Read the live runtime overview.",
                        "expected_result":"A current overview observation with scope and coverage.",
                        "after_initial_tasks":if invalid {json!([1])} else {json!([])}
                    }]
                }),
            )
        } else if attempt.is_none() {
            tool("run_next_work_item", "continue-corrected-work", json!({}))
        } else if attempt.as_ref().is_some_and(|row| row.0 == "running") {
            let selected = feedback
                .iter()
                .any(|row| row["tool_call_id"] == "select-live-overview");
            let observed = feedback
                .iter()
                .any(|row| row["tool_call_id"] == "observe-live-overview");
            if !selected {
                tool(
                    "tool_search",
                    "select-live-overview",
                    json!({"query":"select:introspect"}),
                )
            } else if !observed {
                tool(
                    "invoke_tool",
                    "observe-live-overview",
                    json!({
                        "name":"introspect","arguments":{"facet":"overview","depth":"summary","format":"json"}
                    }),
                )
            } else {
                tool(
                    "settle_work_item",
                    "deliver-corrected-work",
                    json!({
                        "outcome":"delivered","summary":"Observed the current runtime overview with its scope and coverage."
                    }),
                )
            }
        } else {
            json!({"role":"assistant","content":"The runtime overview was observed and the corrected task was delivered."})
        };
        native_work_provider_response(&request, 3, message)
    }

    let pool = setup_lifecycle_run_db_it().await;
    let owner = format!("work-input-owner-{}", Uuid::new_v4());
    let session = format!("work-input-session-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status) VALUES (?, ?, 'active')")
        .bind(&owner)
        .bind(&session)
        .execute(pool.get())
        .await
        .unwrap();
    let probe = Probe {
        pool: pool.clone(),
        owner: owner.clone(),
        invalid_first,
        calls: Arc::new(AtomicUsize::new(0)),
        observations: Arc::new(TokioMutex::new(Vec::new())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let state = probe.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/chat/completions", post(respond))
                .with_state(state),
        )
        .await
        .unwrap();
    });
    let _provider = TerminalTestLlm {
        base_url: base_url.clone(),
        requests: probe.calls.clone(),
        server,
    };
    let offering = format!("work-input-offering-{}", Uuid::new_v4());
    let model = format!("work-input-model-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO infra_llm_models (model_id, model_name, provider, api_key_encrypted, base_url, is_active, context_window, input_modalities, output_modalities, supported_parameters, pricing, tags, quirks) VALUES (?, ?, 'openai', ?, ?, 1, 128000, ?, ?, ?, ?, ?, ?)")
        .bind(&offering).bind(&model).bind(test_encryptor().encrypt("fixture-key").unwrap())
        .bind(&base_url).bind(r#"["text"]"#).bind(r#"["text"]"#)
        .bind("[]").bind("{}").bind("[]").bind("{}")
        .execute(pool.get()).await.unwrap();
    let service =
        db_backed_test_service(&pool, "work-input-test-owner").with_model_service(Arc::new(
            astra_services::DatabaseModelService::new(pool.settings().clone(), test_encryptor())
                .with_pool(pool.clone()),
        ));
    let mut request = test_request(
        "Use Work to observe the live runtime overview and deliver the declared task.",
    );
    request.model = Some(model);
    request.model_selection = Some(ModelSelection {
        offering_id: offering.clone(),
    });
    request.session_id = Some(session.clone());
    let binding = astra_services::SessionExecutionBindingV1::server_work_default(format!(
        "session:{session}:branch:{}",
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID
    ));
    request.workspace_binding = Some(binding.workspace);
    request.executor_binding = Some(binding.executor);
    request.execution_binding_generation = Some(binding.generation);
    let run = service
        .create_run(owner.clone(), request)
        .await
        .expect("native run admission");
    tokio::time::timeout(Duration::from_secs(60), async {
        while service.background_task_count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native run must release its execution owners");
    let terminal: (String, Option<String>) = sqlx::query_as(
        "SELECT status, error_message FROM agent_runs WHERE user_id = ? AND run_id = ?",
    )
    .bind(&owner)
    .bind(&run.run_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    let observations = probe.observations.lock().await.clone();
    eprintln!(
        "WORK_INPUT_NATIVE_REPLAY {}",
        json!({
            "invalid_first":invalid_first,"terminal":terminal,"requests":observations
        })
    );
    assert_eq!(
        terminal.0, "completed",
        "native correction was stranded: {terminal:?}"
    );
    let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM works WHERE owner_id = ?")
        .bind(&owner)
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert_eq!(works, 1, "only the corrected graph may create Work");
    let operations: Vec<(String, String)> = sqlx::query_as(
        "SELECT operation_state, payload_json FROM work_establishment_operations WHERE owner_id = ?")
        .bind(&owner).fetch_all(pool.get()).await.unwrap();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].0, "complete");
    let payload: Value = serde_json::from_str(&operations[0].1).unwrap();
    let tasks: Vec<astra_services::WorkAdmissionTask> =
        serde_json::from_value(payload["arguments"]["tasks"].clone()).unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].after_initial_tasks.is_empty());
    let attempts: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT status, outcome, executor_run_id FROM work_item_attempts WHERE owner_id = ?",
    )
    .bind(&owner)
    .fetch_all(pool.get())
    .await
    .unwrap();
    assert_eq!(
        attempts,
        vec![("completed".into(), Some("delivered".into()), run.run_id)]
    );
    if invalid_first {
        assert_eq!(
            observations[1]["works"], 0,
            "invalid call cannot create Work"
        );
        assert_eq!(
            observations[1]["operations"], 0,
            "invalid input cannot admit a durable operation"
        );
        let final_feedback = observations.last().unwrap()["feedback"].as_array().unwrap();
        assert_eq!(
            final_feedback
                .iter()
                .filter(|row| row["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("initial Work task cannot depend on itself")))
                .count(),
            1,
            "the invalid carrier must be rejected once, not replayed"
        );
        assert!(
            final_feedback
                .iter()
                .any(|row| row["tool_call_id"] == "corrected-work-input")
        );
    }
    assert!(
        observations
            .iter()
            .any(|row| row["feedback"].as_array().unwrap().iter().any(
                |result| result["tool_call_id"] == "observe-live-overview"
                    && result["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("astra-introspect-model-projection-v1"))
            )),
        "the requested task must obtain actual runtime evidence"
    );
    crate::server::work_test_support::cleanup_work_owner(&pool, &owner).await;
    sqlx::query("DELETE FROM infra_llm_models WHERE model_id = ?")
        .bind(&offering)
        .execute(pool.get())
        .await
        .unwrap();
    pool.close().await;
}
