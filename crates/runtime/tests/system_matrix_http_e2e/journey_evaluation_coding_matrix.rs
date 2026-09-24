//! Public Evaluation journey using a real `astra-edge` process.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use astra_runtime_env::ASTRA_LOCAL_STATE_ROOT_ENV;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokio::process::{Child, Command};

use super::harness::{
    bootstrap, cleanup_edge_registry, get_json, offering_id_from_model_response, post_json,
};

struct EdgeProcess {
    child: Child,
    stderr_path: PathBuf,
}

impl Drop for EdgeProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn run_git(workspace: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .expect("run Git for evaluation fixture");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git output UTF-8")
        .trim()
        .to_string()
}

fn configured_workspace() -> (PathBuf, String) {
    let workspace = std::env::var_os("ASTRA_EVALUATION_EDGE_WORKSPACE_DIR")
        .map(PathBuf::from)
        .expect("ASTRA_EVALUATION_EDGE_WORKSPACE_DIR must name the dedicated source mount");
    assert!(
        workspace.is_dir(),
        "missing dedicated source mount: {}",
        workspace.display()
    );
    let source_commit = run_git(&workspace, &["rev-parse", "HEAD"]);
    let status = run_git(
        &workspace,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    assert!(
        status.is_empty(),
        "dedicated source mount must be a clean checkout before the journey"
    );
    (workspace, source_commit)
}

async fn spawn_provider(skill_name: String) -> String {
    async fn completion(
        State(skill_name): State<String>,
        Json(request): Json<Value>,
    ) -> impl IntoResponse {
        let stream = request.get("stream").and_then(Value::as_bool) == Some(true);
        let tools_empty = request
            .get("tools")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
        if request.get("tool_choice").and_then(Value::as_str) == Some("none") && tools_empty {
            let typed_judgment = request
                .get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|content| content.contains("\"questions\""))
                    })
                });
            if typed_judgment {
                return sse_text(
                    r#"{"true":["mutation.must_mutate","scope.workspace"],"uncertain":[]}"#,
                )
                .into_response();
            }
            let decision = json!({
                "work_lifecycle": "not_required",
                "workspace_mutation": "must_mutate",
                "mutation_completion_scope": "workspace",
                "execution_topology": "primary",
                "required_capabilities": []
            })
            .to_string();
            return sse_text(&decision).into_response();
        }
        if !stream {
            return Json(json!({
                "choices": [{"message": {"content": "probe ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))
            .into_response();
        }
        let messages = request
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tool_outputs = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if tool_outputs.len() >= 3 {
            return sse_text("identical final response").into_response();
        }
        if tool_outputs
            .iter()
            .any(|output| output.contains("Successfully wrote") || output.contains("written"))
        {
            return sse_tool_call("evaluation-read", "read_file", json!({"path":"answer.txt"}))
                .into_response();
        }
        if let Some(skill_output) = tool_outputs
            .iter()
            .find(|output| output.contains("EVAL_BASELINE") || output.contains("EVAL_CANDIDATE"))
        {
            let content = if skill_output.contains("EVAL_CANDIDATE") {
                "right\n"
            } else {
                "wrong\n"
            };
            return sse_tool_call(
                "evaluation-write",
                "write_file",
                json!({"path":"answer.txt","content":content}),
            )
            .into_response();
        }
        sse_tool_call(
            "evaluation-skill",
            "skill",
            json!({"skill_name": skill_name}),
        )
        .into_response()
    }

    fn sse_text(content: &str) -> ([(axum::http::HeaderName, &'static str); 1], String) {
        let delta = json!({"choices":[{"delta":{"content":content}}]});
        let terminal = json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2}});
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            format!("data: {delta}\n\ndata: {terminal}\n\ndata: [DONE]\n\n"),
        )
    }

    fn sse_tool_call(
        id: &str,
        name: &str,
        arguments: Value,
    ) -> ([(axum::http::HeaderName, &'static str); 1], String) {
        let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{
            "index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}
        }]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":4,"completion_tokens":2}});
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            format!("data: {delta}\n\ndata: [DONE]\n\n"),
        )
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind evaluation provider");
    let addr = listener.local_addr().expect("evaluation provider address");
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/chat/completions", post(completion))
                .with_state(skill_name),
        )
        .await
        .expect("serve evaluation provider");
    });
    format!("http://{addr}/v1")
}

async fn wait_for_edge(app: &Router, auth: &str, edge_id: &str, edge: &mut EdgeProcess) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = edge.child.try_wait().expect("inspect Edge process") {
            let stderr = std::fs::read_to_string(&edge.stderr_path).unwrap_or_default();
            panic!("astra-edge exited before registration ({status}): {stderr}");
        }
        let (status, body) = get_json(app, "/edges/status", Some(auth), &[]).await;
        if status == StatusCode::OK
            && body["edges"].as_array().is_some_and(|edges| {
                edges
                    .iter()
                    .any(|edge| edge["edge_agent_id"].as_str() == Some(edge_id))
            })
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Edge did not register: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_observation(app: &Router, auth: &str, experiment_id: &str, count: u64) -> Value {
    let path = format!("/evaluation/experiments/{experiment_id}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let (status, projection) = get_json(app, &path, Some(auth), &[]).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "evaluation projection: {projection}"
        );
        if projection["observed_trial_count"].as_u64() == Some(count) {
            return projection;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "evaluation did not reach {count} observations: {projection}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn run_evaluation_coding_real_edge() {
    let bootstrap = bootstrap().await;
    let ctx = &bootstrap.ctx;
    cleanup_edge_registry(&ctx.pool, &ctx.user_id, &ctx.edge_agent_id).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Evaluation network server");
    let addr = listener.local_addr().expect("Evaluation server address");
    let server = tokio::spawn({
        let app = ctx.app.clone();
        async move {
            axum::serve(listener, app)
                .await
                .expect("serve Evaluation app")
        }
    });

    let evaluation_config = std::env::var_os("ASTRA_EVALUATION_EDGE_CONFIG")
        .map(PathBuf::from)
        .expect("ASTRA_EVALUATION_EDGE_CONFIG must name the dedicated deployment config");
    assert!(
        evaluation_config.is_file(),
        "missing dedicated evaluation config: {}",
        evaluation_config.display()
    );
    let (workspace, source_commit) = configured_workspace();
    let edge_bin = std::env::var_os("ASTRA_EVALUATION_EDGE_BIN")
        .map(PathBuf::from)
        .expect("ASTRA_EVALUATION_EDGE_BIN must name the built astra-edge binary");
    assert!(
        edge_bin.is_file(),
        "missing astra-edge: {}",
        edge_bin.display()
    );
    let process_root = tempfile::tempdir().expect("Edge local state");
    let stderr_path = process_root.path().join("edge.stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("Edge stderr log");
    let token = bootstrap
        .auth_header
        .strip_prefix("Bearer ")
        .expect("Bearer auth header");
    let child = Command::new(edge_bin)
        .arg("--server-url")
        .arg(format!("http://{addr}"))
        .arg("--token")
        .arg(token)
        .arg("--evaluation-config")
        .arg(&evaluation_config)
        .arg("--workspace-dir")
        .arg(&workspace)
        .arg("--edge-id")
        .arg(&ctx.edge_agent_id)
        .arg("--reconnect=false")
        .env(ASTRA_LOCAL_STATE_ROOT_ENV, process_root.path())
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn astra-edge");
    let mut edge = EdgeProcess { child, stderr_path };
    wait_for_edge(
        &ctx.app,
        &bootstrap.auth_header,
        &ctx.edge_agent_id,
        &mut edge,
    )
    .await;

    let skill_name = format!("coding-evaluation-{}", ctx.suffix);
    let provider_url = spawn_provider(skill_name.clone()).await;
    let model_name = format!("evaluation-coding-{}", ctx.suffix);
    let (model_status, model) = post_json(
        &ctx.app,
        "/models",
        Some(&bootstrap.auth_header),
        json!({
            "name": model_name,
            "provider": "openai",
            "context_window": 128000,
            "api_key": "evaluation-e2e-key",
            "base_url": provider_url,
            "pricing": {
                "prompt": 0.000001,
                "completion": 0.000002,
                "cache_read": 0.0000005,
                "cache_write": 0.000001
            }
        }),
    )
    .await;
    assert_eq!(
        model_status,
        StatusCode::CREATED,
        "create Evaluation model: {model}"
    );
    let offering_id = offering_id_from_model_response(&model).to_string();
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_id = ?")
        .bind(&offering_id)
        .execute(&ctx.pool)
        .await
        .expect("activate Evaluation model");
    astra_services::models::invalidate_active_llm_model_resolution_cache();

    let (source_status, source) = post_json(
        &ctx.app,
        "/skills/user",
        Some(&bootstrap.auth_header),
        json!({"skill_name":skill_name,"visibility":"private"}),
    )
    .await;
    assert_eq!(source_status, StatusCode::CREATED, "create Skill: {source}");
    let manifest = json!({
        "name": skill_name,
        "description": "Evaluation coding fixture",
        "execution_context": "inline",
        "allowed_tools": [],
        "required_capabilities": []
    });
    let mut revision_ids = Vec::new();
    for (version, marker) in [("1.0.0", "EVAL_BASELINE"), ("2.0.0", "EVAL_CANDIDATE")] {
        let (status, revision) = post_json(
            &ctx.app,
            &format!("/skills/user/{skill_name}/versions"),
            Some(&bootstrap.auth_header),
            json!({
                "version":version,
                "manifest_json":manifest,
                "content_markdown":format!("Return the marker {marker}."),
                "status":"published"
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "publish Skill revision: {revision}"
        );
        revision_ids.push(
            revision["version_id"]
                .as_str()
                .expect("Skill version_id")
                .to_string(),
        );
    }

    let prepare_request = json!({
        "submission_idempotency_key":format!("coding-e2e-{}",ctx.suffix),
        "target":{
            "kind":"skill",
            "skill_name":skill_name,
            "baseline":{"revision_id":revision_ids[0]},
            "candidate":{"revision_id":revision_ids[1]}
        },
        "case":{
            "case_id":"coding-case",
            "message":"Use the pinned Skill and update answer.txt.",
            "verifier_config":{
                "kind":"workspace_command",
                "command":"test \"$(cat answer.txt)\" = right",
                "expected_exit_code":0,
                "timeout_secs":30
            },
            "holdout":false
        },
        "model_offering_id":offering_id,
        "workspace":{
            "edge_executor_id":ctx.edge_agent_id,
            "source_commit":source_commit,
            "tool_names":["write_file","read_file"]
        },
        "max_concurrency":1,
        "max_wall_time_secs":90
    });
    let (prepare_status, prepared) = post_json(
        &ctx.app,
        "/evaluation/experiments/prepare",
        Some(&bootstrap.auth_header),
        prepare_request.clone(),
    )
    .await;
    assert_eq!(
        prepare_status,
        StatusCode::OK,
        "prepare Evaluation: {prepared}"
    );
    let (replay_status, replay) = post_json(
        &ctx.app,
        "/evaluation/experiments/prepare",
        Some(&bootstrap.auth_header),
        prepare_request,
    )
    .await;
    assert_eq!(replay_status, StatusCode::OK);
    assert_eq!(replay, prepared, "prepare replay must preserve identities");
    let experiment_id = prepared["experiment"]["experiment_id"]
        .as_str()
        .expect("experiment id")
        .to_string();
    let trials = prepared["trials"].as_array().expect("prepared trials");
    assert_eq!(trials.len(), 2);

    let mut outcomes = Vec::new();
    for (index, trial) in trials.iter().enumerate() {
        let trial_id = trial["trial_id"].as_str().expect("trial id");
        let start_path = format!("/evaluation/experiments/{experiment_id}/trials/{trial_id}/start");
        let (status, started) = post_json(
            &ctx.app,
            &start_path,
            Some(&bootstrap.auth_header),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "start trial: {started}");
        let (replay_status, replay_started) = post_json(
            &ctx.app,
            &start_path,
            Some(&bootstrap.auth_header),
            json!({}),
        )
        .await;
        assert_eq!(
            replay_status,
            StatusCode::ACCEPTED,
            "replay trial start: {replay_started}"
        );
        assert_eq!(replay_started["session_id"], started["session_id"]);
        assert_eq!(replay_started["run_id"], started["run_id"]);
        wait_for_observation(
            &ctx.app,
            &bootstrap.auth_header,
            &experiment_id,
            (index + 1) as u64,
        )
        .await;
        let (assessment_status, assessment) = post_json(
            &ctx.app,
            &format!("/evaluation/experiments/{experiment_id}/trials/{trial_id}/assess"),
            Some(&bootstrap.auth_header),
            json!({}),
        )
        .await;
        assert_eq!(
            assessment_status,
            StatusCode::OK,
            "assess trial: {assessment}"
        );
        outcomes.push(
            assessment["assessment"]["outcome"]["status"]
                .as_str()
                .expect("assessment status")
                .to_string(),
        );
        let session_id = started["session_id"].as_str().expect("trial session id");
        let artifact_id = format!("evaluation-coding-{trial_id}");
        let (artifact_status, artifact) = get_json(
            &ctx.app,
            &format!("/sessions/{session_id}/artifacts/{artifact_id}"),
            Some(&bootstrap.auth_header),
            &[],
        )
        .await;
        assert_eq!(
            artifact_status,
            StatusCode::OK,
            "coding artifact: {artifact}"
        );
        assert_eq!(artifact["artifact_kind"], "evaluation_coding_evidence");
        assert!(artifact["content"]["patch"].is_string());
        assert_eq!(artifact["content"]["isolation"]["namespace_active"], true);
        assert_eq!(artifact["content"]["isolation"]["scope_settled"], true);
    }
    assert_eq!(outcomes, ["fail", "pass"]);
    assert_eq!(
        std::fs::read_to_string(workspace.join("answer.txt")).expect("base answer"),
        "wrong\n",
        "trial clones must not mutate the registered source checkout"
    );

    let (report_status, report) = get_json(
        &ctx.app,
        &format!("/evaluation/experiments/{experiment_id}/report"),
        Some(&bootstrap.auth_header),
        &[],
    )
    .await;
    assert_eq!(report_status, StatusCode::OK, "Evaluation report: {report}");
    assert_eq!(report["manifest"]["coverage"]["planned_trial_count"], 2);
    assert_eq!(report["manifest"]["coverage"]["observed_trial_count"], 2);
    assert_eq!(report["manifest"]["coverage"]["evidence_incomplete"], false);

    let coding_artifacts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifacts WHERE user_id = ? AND artifact_kind = 'evaluation_coding_evidence'",
    )
    .bind(&ctx.user_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("count coding artifacts");
    assert_eq!(coding_artifacts, 2);
    let dispatches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM edge_pending_dispatch WHERE user_id = ? AND edge_agent_id = ? AND status = 'completed'",
    )
    .bind(&ctx.user_id)
    .bind(&ctx.edge_agent_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("count Edge dispatches");
    assert!(dispatches >= 2, "expected actual Edge tool dispatches");

    edge.child.kill().await.expect("stop astra-edge");
    let _ = edge.child.wait().await;
    server.abort();
    sqlx::query("DELETE FROM infra_llm_models WHERE model_id = ?")
        .bind(&offering_id)
        .execute(&ctx.pool)
        .await
        .expect("remove Evaluation model");
    bootstrap.ctx.close().await;
}
