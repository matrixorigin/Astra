use super::{fanout_test_context, test_executor, test_spawner};
use crate::edge_tools::{ToolExecutor, all_tool_schemas, truncate_output};
use astra_services::session_journal::{self, JournalDirGuard, JournalEvent, JournalEventType};
use astra_services::session_workspace::{self, ContextTraceSignal, WorkspaceMetadata};
use chrono::Utc;
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── ToolExecutor ──────────────────────────────────────────────────────────

#[test]
fn executor_tool_count_matches_schemas() {
    let executor = test_executor();
    assert_eq!(executor.tool_count(), all_tool_schemas().len());
}

#[test]
fn executor_tool_names_match_schemas() {
    let executor = test_executor();
    let names = executor.tool_names();
    assert_eq!(names.len(), all_tool_schemas().len());
    assert!(names.contains(&"bash".to_string()));
}

#[tokio::test]
async fn execute_unknown_tool_returns_error() {
    let executor = test_executor();
    let result = executor.execute("nonexistent_tool", &json!({})).await;
    assert!(
        result.starts_with("Error:"),
        "unknown tool must return an Error: prefix (plain-text error contract) — got: {result}"
    );
    assert!(
        result.contains("nonexistent_tool"),
        "error must name the rejected tool — got: {result}"
    );
    assert!(
        result.contains("not available"),
        "error must use the canonical 'not available' phrasing so dispatchers can pattern-match — got: {result}"
    );
}

#[tokio::test]
async fn git_github_helper_style_names_are_unknown_on_cli_edge_executor() {
    let executor = test_executor();

    let git_actions = [
        "status",
        "diff",
        "log",
        "show",
        "blame",
        "file_history",
        "log_search",
        "contributors",
        "commit",
        "revert_commit",
        "stash",
        "checkout_file",
        "worktree",
    ];
    let github_actions = [
        "list_prs",
        "get_pr",
        "ci_status",
        "list_issues",
        "get_issue",
        "repo_stats",
        "create_issue",
    ];

    for name in git_actions
        .into_iter()
        .map(|action| format!("git_{action}"))
        .chain(
            github_actions
                .into_iter()
                .map(|action| format!("github_{action}")),
        )
    {
        let result = executor.execute(&name, &json!({})).await;
        assert!(result.starts_with("Error:"), "{name}: {result}");
        assert!(result.contains("not available"), "{name}: {result}");
    }
}

#[tokio::test]
async fn unsupported_session_state_actions_are_rejected_on_cli_edge_executor() {
    let executor = test_executor();

    for action in [
        "configure_later",
        "wait_until",
        "restore_context",
        "questionnaire",
        "rollback_edits",
        "timeline",
        "summary",
        "history",
    ] {
        let result = executor
            .execute("session", &json!({"action": action}))
            .await;
        assert!(result.starts_with("Error:"), "{action}: {result}");
        assert!(
            result.contains("unknown `session` action")
                && result.contains("history_page")
                && result.contains("rollback_session_state"),
            "{action}: {result}"
        );
    }
}

#[tokio::test]
async fn consolidated_github_create_issue_error_does_not_leak_helper_style_name() {
    let executor = test_executor();

    let result = executor
        .execute(
            "github",
            &json!({
                "action": "create_issue",
                "repo": "not-owner-repo",
                "title": "Fix it"
            }),
        )
        .await;

    assert!(!result.contains("github_"), "{result}");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("github error json");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["tool"], "github");
    assert!(
        parsed["error"]
            .as_str()
            .unwrap_or("")
            .contains("create_issue")
    );
}

/// Standalone `delegate` is not a CLI executor tool. Server/runtime
/// interception must happen before local tool execution; if it reaches
/// this executor, it must fail closed.
#[tokio::test]
async fn execute_delegate_tool_does_not_return_fake_acknowledgment() {
    let executor = test_executor();
    let result = executor.execute("delegate", &json!({})).await;
    assert!(
        result.starts_with("Error"),
        "delegate must fail closed in the CLI executor: {result}"
    );
    assert!(!result.contains("acknowledged"), "got: {result}");
}

#[tokio::test]
async fn execute_with_metadata_marks_structured_str_replace_failure_as_error() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("f.txt"), "let current = 1;\n").unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf());

    let read = executor
        .execute("read_file", &json!({"path": "f.txt"}))
        .await;
    assert!(read.contains("let current = 1;"), "{read}");

    let outcome = executor
        .execute_with_metadata(
            "str_replace",
            &json!({
                "path": "f.txt",
                "old_str": "let stale = 1;",
                "new_str": "let stale = 2;"
            }),
        )
        .await;

    assert!(outcome.is_error, "{outcome:?}");
    assert!(
        outcome.output.contains("STR_REPLACE FAILED"),
        "{}",
        outcome.output
    );
    assert_eq!(
        astra_turn_core::tool_result_semantics::cloud_tool_result_status_label(&outcome.output),
        "failed"
    );
}

#[tokio::test]
async fn execute_with_metadata_bash_non_zero_exit_is_structured_failure() {
    let executor = test_executor();
    let outcome = executor
        .execute_with_metadata_cancelable("bash", &json!({"command": "exit 7"}), None)
        .await;

    assert!(outcome.is_error, "{outcome:?}");
    let fields = outcome.tool_result_fields.expect("metadata fields");
    assert_eq!(
        fields.get("exit_code").and_then(serde_json::Value::as_i64),
        Some(7)
    );
    assert_eq!(
        fields
            .get("exit_semantics")
            .and_then(serde_json::Value::as_str),
        Some("execution_error")
    );
    assert_eq!(
        fields
            .get("result_class")
            .and_then(serde_json::Value::as_str),
        Some("execution_error")
    );
}

#[tokio::test]
async fn execute_with_metadata_bash_non_zero_exit_preserves_structured_failure() {
    let executor = test_executor();
    let outcome = executor
        .execute_with_metadata("bash", &json!({"command": "exit 7"}))
        .await;

    assert!(outcome.is_error, "{outcome:?}");
    let fields = outcome.tool_result_fields.expect("metadata fields");
    assert_eq!(
        fields.get("exit_code").and_then(serde_json::Value::as_i64),
        Some(7)
    );
    assert_eq!(
        fields
            .get("exit_semantics")
            .and_then(serde_json::Value::as_str),
        Some("execution_error")
    );
    assert_eq!(
        fields
            .get("result_class")
            .and_then(serde_json::Value::as_str),
        Some("execution_error")
    );
}

#[tokio::test]
async fn execute_with_metadata_bash_empty_result_is_structured_non_error() {
    let executor = test_executor();
    let outcome = executor
        .execute_with_metadata_cancelable(
            "bash",
            &json!({"command": "printf 'abc\\n' | grep ZZZ_NO_MATCH"}),
            None,
        )
        .await;

    assert!(!outcome.is_error, "{outcome:?}");
    let fields = outcome.tool_result_fields.expect("metadata fields");
    assert_eq!(
        fields.get("exit_code").and_then(serde_json::Value::as_i64),
        Some(1)
    );
    assert_eq!(
        fields
            .get("exit_semantics")
            .and_then(serde_json::Value::as_str),
        Some("empty_result")
    );
    assert_eq!(
        fields
            .get("result_class")
            .and_then(serde_json::Value::as_str),
        Some("empty_result")
    );
}

/// REGRESSION: the consolidated `agent` tool must reject the blocked
/// delegate action instead of returning a successful placeholder.
///
/// The fix is twofold: (1) the schema enum drops "delegate" so the
/// model can't pick it, and (2) defence-in-depth — the executor
/// rejects it as an unknown action with an actionable redirect.
#[tokio::test]
async fn agent_action_delegate_is_rejected_with_redirect_to_spawn() {
    let executor = test_executor().with_spawn_context(fanout_test_context(test_spawner()));
    let result = executor
        .execute(
            "agent",
            &json!({
                "action": "delegate",
                "task": "review HEAD~3..HEAD"
            }),
        )
        .await;
    assert!(
        result.starts_with("Error"),
        "blocked delegate action must return an Error: prefix so the TUI renders \
         it as a failure (red banner), not as a normal tool result. Got: {result}"
    );
    assert!(
        result.contains("spawn"),
        "blocked delegate action error must name the `agent` spawn action as the \
         alternative — without that, the model has no path to recovery. \
         Got: {result}"
    );
    assert!(
        !result.contains("acknowledged"),
        "delegate-shaped calls must not return success-style placeholder text. Got: {result}"
    );
    // End-to-end UX assertion: the Error: prefix must classify through
    // tool_result_semantics::is_tool_error → cloud_tool_result_status_label
    // → "failed", so the TUI renders the red `•` failure banner instead
    // of the green success banner. This is the load-bearing wire that
    // makes the failure visible to the human.
    assert!(
        astra_turn_core::tool_result_semantics::is_tool_error(&result),
        "the new error must be classified as an error by is_tool_error \
         (drives TUI red banner / Failed label). Got: {result}"
    );
    assert_eq!(
        astra_turn_core::tool_result_semantics::cloud_tool_result_status_label(&result),
        "failed",
        "the new error must produce status='failed' for cloud reporting; \
         status='completed' would re-poison the model's belief that the \
         delegation succeeded. Got: {result}"
    );
}

/// REGRESSION (session e15691e5): the model emitted
/// `agent({ "spawn": { ... } })` / `agent({ "spawn": "..." })` instead of
/// the consolidated `agent({ "action": "spawn", ... })` shape. The generic
/// "unknown agent action ''" error was technically correct but not
/// actionable enough — it didn't explain that `spawn` must be the VALUE of
/// `action`, not a wrapper key.
#[tokio::test]
async fn agent_missing_action_with_spawn_wrapper_redirects_to_action_field() {
    let executor = test_executor().with_spawn_context(fanout_test_context(test_spawner()));
    let result = executor
        .execute(
            "agent",
            &json!({
                "spawn": {
                    "description": "Review latest commit",
                    "prompt": "Inspect HEAD~1..HEAD for regressions"
                }
            }),
        )
        .await;
    assert!(result.starts_with("Error:"), "got: {result}");
    assert!(
        result.contains("action='spawn'") || result.contains("\"action\":\"spawn\""),
        "error must show the correct top-level action shape so the model can recover. Got: {result}"
    );
    assert!(
        result.contains("top-level") || result.contains("wrapper"),
        "error must explain that `spawn` is not a wrapper key. Got: {result}"
    );
}

/// `task` is only the durable checklist surface. Background process
/// control belongs to the typed `task_output`/`task_stop`/`task_list`
/// tools; if old background actions arrive on `task`, treat them as
/// ordinary unknown task actions rather than preserving migration branches.
#[tokio::test]
async fn task_background_actions_are_plain_unknown_task_actions() {
    let executor = test_executor();
    for action in ["background_shell", "background_agent", "output", "kill"] {
        let result = executor
            .execute(
                "task",
                &json!({
                    "action": action,
                    "command": "echo hi",
                    "prompt": "hi",
                    "task_id": "bg-shell-1",
                }),
            )
            .await;
        assert!(
            result.starts_with("Error"),
            "task.{action} must return an Error: prefix so the TUI renders \
             a red banner — got: {result}"
        );
        assert!(
            result.contains("unknown `task` action") && result.contains(action),
            "task.{action} must be rejected by the ordinary unknown-action path. Got: {result}"
        );
        assert!(
            astra_turn_core::tool_result_semantics::is_tool_error(&result),
            "task.{action} rejection must classify as an error so cloud \
             reporting marks status='error' and the TUI shows red. \
             Got: {result}"
        );
    }
}

/// Stale session sub-actions are rejected, but the error must still name
/// the dedicated tools so the model can self-correct on the next call.
#[tokio::test]
async fn session_enter_exit_plan_actions_redirect_to_top_level_tools() {
    let executor = test_executor();
    for (action, redirect_tool) in &[
        ("enter_plan", "enter_plan_mode"),
        ("exit_plan", "exit_plan_mode"),
    ] {
        let result = executor
            .execute("session", &json!({"action": action}))
            .await;
        assert!(
            result.starts_with("Error"),
            "session.{action} must return an Error: prefix — got: {result}"
        );
        assert!(
            result.contains("unknown `session` action"),
            "session.{action} should be rejected as unknown. Got: {result}"
        );
        assert!(
            result.contains(redirect_tool),
            "session.{action} error must name `{redirect_tool}` so the model can recover. Got: {result}"
        );
        assert!(
            astra_turn_core::tool_result_semantics::is_tool_error(&result),
            "session.{action} rejection must classify as an error so the \
             TUI shows red. Got: {result}"
        );
    }
}

#[tokio::test]
async fn exit_plan_mode_ignores_model_supplied_approval_without_overlay() {
    // LLM/tool arguments are not a trusted approval source. Even if the
    // model passes `approved: true`, exit_plan_mode must require the
    // interactive plan-review overlay before unlocking writes.
    for (label, use_cloud, status, plan_id) in [
        ("cloud planning", true, "planning", "plan-cloud-plan"),
        ("cloud refining", true, "refining", "plan-cloud-ref"),
        ("offline", false, "planning", ""),
    ] {
        let session_id = format!("sess-{label}");

        if use_cloud {
            let server = MockServer::start().await;
            mock_authoring_plan_present(&server, &session_id, plan_id, status).await;

            let temp = tempfile::tempdir().unwrap();
            let executor = ToolExecutor::new(temp.path().to_path_buf())
                .with_active_session_id(&session_id)
                .with_cloud(server.uri(), "token");

            let result = executor
                .execute(
                    "exit_plan_mode",
                    &json!({"plan": "1. Ship auth", "approved": true}),
                )
                .await;

            assert!(
                result.contains("trusted interactive plan-review overlay"),
                "[{label}] model-supplied approval must not bypass UI review. Got: {result}"
            );
            assert_eq!(
                executor.take_pending_permission_mode_change(),
                None,
                "[{label}] no trusted approval means no permission-mode change"
            );
        } else {
            let temp = tempfile::tempdir().unwrap();
            let executor =
                ToolExecutor::new(temp.path().to_path_buf()).with_active_session_id(&session_id);

            let enter_result = executor
                .execute("enter_plan_mode", &json!({"goal": "Ship auth"}))
                .await;
            assert!(
                !enter_result.starts_with("Error:"),
                "[{label}] offline enter should succeed. Got: {enter_result}"
            );
            assert_eq!(
                executor.take_pending_permission_mode_change(),
                Some(crate::cli::permission_manager::PermissionMode::Plan),
                "[{label}] enter must stage Plan"
            );

            let exit_result = executor
                .execute(
                    "exit_plan_mode",
                    &json!({"approved": true, "plan": "1. Ship auth"}),
                )
                .await;
            assert!(
                exit_result.contains("trusted interactive plan-review overlay"),
                "[{label}] model-supplied approval must not bypass local UI review. Got: {exit_result}"
            );
            assert_eq!(
                executor.take_pending_permission_mode_change(),
                None,
                "[{label}] no trusted approval means local plan mode stays active"
            );
        }
    }
}

#[tokio::test]
async fn plan_mode_write_guard_cache_is_invalidated_when_session_changes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-plan"))
        .and(query_param("active_session_only", "true"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": [{ "plan_id": "plan-1", "goal": "Ship auth", "status": "planning" }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-normal"))
        .and(query_param("active_session_only", "true"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": []
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-plan")
        .with_cloud(server.uri(), "token");

    let blocked = executor
        .execute("bash", &json!({ "command": "true" }))
        .await;
    assert!(
        blocked.contains("blocked while plan mode is active"),
        "first session should populate the authoring cache as blocked. Got: {blocked}"
    );

    executor.set_active_session_id("sess-normal");
    let allowed = executor
        .execute("bash", &json!({ "command": "true" }))
        .await;
    assert!(
        !allowed.contains("blocked while plan mode is active"),
        "switching sessions must not reuse the prior session's plan-mode cache. Got: {allowed}"
    );
}

// ── Overlay-driven exit_plan_mode paths ───────────────────────────
// All three overlay outcomes share the same arrange → act shape:
// exit_plan_mode without `approved` routes through plan_review_request_tx.

#[tokio::test]
async fn exit_plan_mode_overlay_paths() {
    use crate::cli::chat_stream::PlanReviewDecision;
    use crate::cli::permission_manager::PermissionMode;

    #[derive(Debug)]
    struct Case {
        label: &'static str,
        install_overlay: bool,
        decision: Option<PlanReviewDecision>,
        expect_starts_with: &'static str,
        expect_contains: &'static [&'static str],
        expect_not_contains: &'static [&'static str],
        expect_pending_mode: Option<PermissionMode>,
        expect_tool_boost: Option<&'static [&'static str]>,
    }

    let cases = [
        Case {
            label: "approve-auto",
            install_overlay: true,
            decision: Some(PlanReviewDecision::Approve {
                mode: PermissionMode::Auto,
            }),
            expect_starts_with: "Exited plan mode.",
            expect_contains: &["auto"],
            expect_not_contains: &["Error"],
            expect_pending_mode: Some(PermissionMode::Auto),
            expect_tool_boost: Some(&["bash", "read_file", "write_file", "str_replace"]),
        },
        Case {
            label: "keep-planning",
            install_overlay: true,
            decision: Some(PlanReviewDecision::KeepPlanning),
            expect_starts_with: "",
            expect_contains: &["left open", "feedback"],
            expect_not_contains: &["Error"],
            expect_pending_mode: None,
            expect_tool_boost: None,
        },
        Case {
            label: "no-overlay",
            install_overlay: false,
            decision: None,
            expect_starts_with: "Error:",
            expect_contains: &["trusted interactive plan-review overlay"],
            expect_not_contains: &[],
            expect_pending_mode: None,
            expect_tool_boost: None,
        },
    ];

    for case in &cases {
        let plan_id = format!("plan-{}-{}", case.label, Utc::now().timestamp_millis());
        let plan_text = if case.label == "keep-planning" {
            "1. Draft"
        } else {
            "1. Ship auth"
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("active_session_only", "true"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plans": [{ "plan_id": &plan_id, "goal": "Ship auth", "status": "planning" }]
            })))
            .mount(&server)
            .await;

        // Only approve / keep-planning paths hit the POST endpoint
        if case.install_overlay {
            let mock_approved = matches!(case.decision, Some(PlanReviewDecision::Approve { .. }));
            Mock::given(method("POST"))
                .and(path(format!("/plans/{plan_id}/exit-plan-mode")))
                .and(body_json(json!({
                    "approved": mock_approved,
                    "plan_md": plan_text
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "plan_id": &plan_id,
                    "phase": "refining"
                })))
                .mount(&server)
                .await;
        }

        let temp = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(temp.path().to_path_buf())
            .with_active_session_id("sess-1")
            .with_cloud(server.uri(), "token");

        let mut overlay_task = None;
        if let Some(decision) = &case.decision {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<
                crate::cli::chat_stream::PlanReviewRequest,
            >();
            executor.set_plan_review_request_tx(Some(tx));
            let dec = decision.clone();
            overlay_task = Some(tokio::spawn(async move {
                let request = rx.recv().await.expect("overlay request");
                let _ = request.response_tx.send(dec);
            }));
        }

        let result = executor
            .execute("exit_plan_mode", &json!({"plan": plan_text}))
            .await;

        if let Some(task) = overlay_task {
            task.await.unwrap();
        }

        if !case.expect_starts_with.is_empty() {
            assert!(
                result.starts_with(case.expect_starts_with),
                "[{}] expected start '{}'. Got: {result}",
                case.label,
                case.expect_starts_with
            );
        }
        for expect in case.expect_contains {
            assert!(
                result.contains(expect),
                "[{}] expected to contain '{}'. Got: {result}",
                case.label,
                expect
            );
        }
        for expect_not in case.expect_not_contains {
            assert!(
                !result.contains(expect_not),
                "[{}] expected NOT to contain '{}'. Got: {result}",
                case.label,
                expect_not
            );
        }
        assert_eq!(
            executor.take_pending_permission_mode_change(),
            case.expect_pending_mode,
            "[{}] pending permission mode mismatch",
            case.label
        );
        let expected_boost: Option<Vec<String>> = case
            .expect_tool_boost
            .map(|v| v.iter().map(|s| s.to_string()).collect());
        assert_eq!(
            executor.take_pending_round_tool_boost(),
            expected_boost,
            "[{}] tool boost mismatch",
            case.label
        );
    }
}

// ── Plan-mode entry invariants ──────────────────────────────────────
// enter_plan_mode must stage PermissionMode::Plan regardless of cloud availability.

#[tokio::test]
async fn enter_plan_mode_stages_permission_mode_plan() {
    for (label, use_cloud) in [("offline", false), ("cloud", true)] {
        let temp = tempfile::tempdir().unwrap();

        let executor = if use_cloud {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/plans"))
                .and(header("authorization", "Bearer token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "plan_id": "plan-cloud-1",
                    "phase": "planning"
                })))
                .mount(&server)
                .await;
            ToolExecutor::new(temp.path().to_path_buf())
                .with_active_session_id("sess-cloud")
                .with_cloud(server.uri(), "token")
        } else {
            ToolExecutor::new(temp.path().to_path_buf()).with_active_session_id("sess-offline")
        };

        let result = executor
            .execute("enter_plan_mode", &json!({"goal": "Ship auth"}))
            .await;

        assert!(
            !result.starts_with("Error:"),
            "[{label}] enter_plan_mode must not error. Got: {result}"
        );
        assert_eq!(
            executor.take_pending_permission_mode_change(),
            Some(crate::cli::permission_manager::PermissionMode::Plan),
            "[{label}] must stage Plan on the pending permission-mode slot"
        );
    }
}

#[tokio::test]
async fn exit_plan_mode_local_path_makes_zero_cloud_calls() {
    // Invariant I1 reinforcement: explicit assertion that the local
    // path makes ZERO calls to cloud plan endpoints. Today the path
    // does a probe `GET /plans?phase=planning` which is borderline
    // acceptable for fall-back detection, but should not hit
    // `POST /plans/*/exit-plan-mode`. We rely on wiremock's "no
    // unmounted endpoint matched" semantics: any unexpected request
    // produces a 404 the test can observe via the result string.
    //
    // GREEN once Step 4 finishes (already partially correct via
    // dual-path Step 3 work).
    use crate::cli::chat_stream::PlanReviewDecision;
    use crate::cli::permission_manager::PermissionMode;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-no-cloud"))
        .and(query_param("active_session_only", "true"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"plans": []})))
        .mount(&server)
        .await;
    // Intentionally NO mock for `POST /plans/*/exit-plan-mode`.

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-no-cloud")
        .with_cloud(server.uri(), "token");

    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::cli::chat_stream::PlanReviewRequest>();
    executor.set_plan_review_request_tx(Some(tx));

    let overlay_task = tokio::spawn(async move {
        let request = rx.recv().await.expect("overlay request");
        let _ = request.response_tx.send(PlanReviewDecision::Approve {
            mode: PermissionMode::Prompt,
        });
    });

    let result = executor
        .execute("exit_plan_mode", &json!({"plan": "1. Investigate"}))
        .await;
    overlay_task.await.unwrap();

    assert!(
        !result.contains("404")
            && !result.contains("failed to exit plan mode")
            && !result.contains("Mock"),
        "local path must not hit any unmounted plan-exit endpoint. Got: {result}"
    );
    assert!(
        result.starts_with("Exited plan mode"),
        "local path should report success without server confirmation. Got: {result}"
    );
    assert_eq!(
        executor.take_pending_permission_mode_change(),
        Some(crate::cli::permission_manager::PermissionMode::Prompt),
        "Prompt approval must be staged for the next round"
    );
    assert_eq!(
        executor.take_pending_round_tool_boost(),
        Some(vec![
            "bash".to_string(),
            "read_file".to_string(),
            "write_file".to_string(),
            "str_replace".to_string(),
        ]),
        "Prompt approval must still restore the core execution tool schemas"
    );
}

#[tokio::test]
async fn enter_plan_mode_then_exit_full_cycle_offline() {
    // End-to-end Shift+Tab parity: enter → exit cycle works without
    // any cloud at all. Asserts the slot transitions cleanly:
    //   1. enter_plan_mode → pending = Some(Plan)
    //   2. host applies → perm_manager simulated as Plan
    //   3. user requests exit → overlay → pending = Some(Auto)
    //   4. host applies → perm_manager simulated as Auto
    //
    // RED until Step 4-3 lets enter_plan_mode work offline.
    let temp = tempfile::tempdir().unwrap();
    let executor =
        ToolExecutor::new(temp.path().to_path_buf()).with_active_session_id("sess-cycle");

    // Step 1: enter
    let enter_result = executor
        .execute("enter_plan_mode", &json!({"goal": "Ship auth"}))
        .await;
    assert!(
        !enter_result.starts_with("Error:"),
        "offline enter should succeed. Got: {enter_result}"
    );
    assert_eq!(
        executor.take_pending_permission_mode_change(),
        Some(crate::cli::permission_manager::PermissionMode::Plan),
        "enter must stage Plan"
    );

    // Step 2-4: exit through overlay, choose Auto
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::cli::chat_stream::PlanReviewRequest>();
    executor.set_plan_review_request_tx(Some(tx));

    let overlay_task = tokio::spawn(async move {
        let request = rx.recv().await.expect("overlay request");
        let _ = request
            .response_tx
            .send(crate::cli::chat_stream::PlanReviewDecision::Approve {
                mode: crate::cli::permission_manager::PermissionMode::Auto,
            });
    });

    let exit_result = executor
        .execute("exit_plan_mode", &json!({"plan": "1. Done"}))
        .await;
    overlay_task.await.unwrap();

    assert!(
        exit_result.starts_with("Exited plan mode"),
        "offline exit should succeed. Got: {exit_result}"
    );
    assert_eq!(
        executor.take_pending_permission_mode_change(),
        Some(crate::cli::permission_manager::PermissionMode::Auto),
        "exit must stage Auto"
    );
}

// ── Plan-mode write guard (CLI parity with server-side guard) ──────────
//
// Session b4cef5bb (2026-05-16, Haiku 4.5) showed the model writing 14
// files via `write_file` while the active plan was still in `planning`
// phase (rejected v2/v3, never approved). The server tool executor
// already short-circuits mutation tools while a plan is being authored
// (`server_tool_executor::is_plan_mode_blocked_tool` +
// `plan_mode_authoring_active`), but the CLI's local `ToolExecutor`
// did NOT. These tests pin the parity contract.

async fn mock_authoring_plan_present(
    server: &MockServer,
    session_id: &str,
    plan_id: &str,
    status: &str,
) {
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(header("authorization", "Bearer token"))
        .and(query_param("session_id", session_id))
        .and(query_param("active_session_only", "true"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": [
                { "plan_id": plan_id, "goal": "Ship auth", "status": status }
            ]
        })))
        .mount(server)
        .await;
}

async fn mock_planning_plan_present(server: &MockServer, session_id: &str, plan_id: &str) {
    mock_authoring_plan_present(server, session_id, plan_id, "planning").await;
}

async fn mock_no_authoring_plan(server: &MockServer, session_id: &str) {
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(header("authorization", "Bearer token"))
        .and(query_param("session_id", session_id))
        .and(query_param("active_session_only", "true"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"plans": []})))
        .mount(server)
        .await;
}

async fn setup_blocked_executor(
    plan_status: &str,
) -> (MockServer, ToolExecutor, tempfile::TempDir) {
    let server = MockServer::start().await;
    mock_authoring_plan_present(&server, "sess-1", "plan-9", plan_status).await;
    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");
    (server, executor, temp)
}

#[test]
fn plan_mode_background_task_guard_blocks_stop_but_allows_reads() {
    assert!(crate::edge_tools::is_plan_mode_blocked_tool(
        "task_stop",
        &json!({"task_id": "bg-shell-1"})
    ));
    assert!(!crate::edge_tools::is_plan_mode_blocked_tool(
        "task_output",
        &json!({"task_id": "bg-shell-1"})
    ));
    assert!(!crate::edge_tools::is_plan_mode_blocked_tool(
        "task_list",
        &json!({})
    ));
}

#[test]
fn plan_mode_guard_is_action_aware_for_git_github_and_memory() {
    for action in ["commit", "revert_commit", "stash", "push"] {
        assert!(
            crate::edge_tools::is_plan_mode_blocked_tool("git", &json!({"action": action})),
            "git(action={action}) must be blocked during plan authoring"
        );
    }

    for action in ["status", "diff", "log", "show", "blame"] {
        assert!(
            !crate::edge_tools::is_plan_mode_blocked_tool("git", &json!({"action": action})),
            "git(action={action}) must stay available during plan authoring"
        );
    }

    assert!(crate::edge_tools::is_plan_mode_blocked_tool(
        "github",
        &json!({"action": "create_issue"})
    ));
    assert!(!crate::edge_tools::is_plan_mode_blocked_tool(
        "github",
        &json!({"action": "list_prs"})
    ));

    assert!(!crate::edge_tools::is_plan_mode_blocked_tool(
        "memory",
        &json!({"action": "recall", "query": "release notes"})
    ));
    assert!(!crate::edge_tools::is_plan_mode_blocked_tool(
        "memory",
        &json!({"action": "remember", "content": "draft plan context"})
    ));
}

#[tokio::test]
async fn read_only_tools_are_not_blocked_while_plan_mode_is_authoring() {
    let server = MockServer::start().await;
    mock_planning_plan_present(&server, "sess-1", "plan-9").await;

    let temp = tempfile::tempdir().unwrap();
    let probe = temp.path().join("probe.txt");
    std::fs::write(&probe, "hello").unwrap();

    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");

    let result = executor
        .execute("read_file", &json!({"path": probe.to_string_lossy()}))
        .await;

    assert!(
        !result.contains("blocked while plan mode is active"),
        "read_file is read-only and must remain available during plan authoring. Got: {result}"
    );
    assert!(
        result.contains("hello"),
        "read_file must return the file contents. Got: {result}"
    );
}

#[tokio::test]
async fn writes_are_unblocked_after_exit_plan_mode_approved() {
    let server = MockServer::start().await;
    // Initially a planning plan exists. exit_plan_mode flips the phase.
    mock_planning_plan_present(&server, "sess-1", "plan-9").await;
    Mock::given(method("POST"))
        .and(path("/plans/plan-9/exit-plan-mode"))
        .and(header("authorization", "Bearer token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_id": "plan-9",
            "phase": "refining"
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("note.txt");
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");

    // Sanity: blocked first.
    let blocked = executor
        .execute(
            "write_file",
            &json!({"path": target.to_string_lossy(), "content": "x"}),
        )
        .await;
    assert!(
        blocked.contains("blocked while plan mode is active"),
        "precondition: writes must be blocked before exit. Got: {blocked}"
    );

    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::cli::chat_stream::PlanReviewRequest>();
    executor.set_plan_review_request_tx(Some(tx));
    let overlay_task = tokio::spawn(async move {
        let request = rx.recv().await.expect("overlay request");
        let _ = request
            .response_tx
            .send(crate::cli::chat_stream::PlanReviewDecision::Approve {
                mode: crate::cli::permission_manager::PermissionMode::Auto,
            });
    });

    // Approve exit through the trusted overlay.
    let exit = executor
        .execute("exit_plan_mode", &json!({"plan": "1. Ship"}))
        .await;
    overlay_task.await.unwrap();
    assert!(
        exit.starts_with("Exited plan mode."),
        "trusted overlay approval must succeed. Got: {exit}"
    );

    // After approval, writes go through.
    let unblocked = executor
        .execute(
            "write_file",
            &json!({"path": target.to_string_lossy(), "content": "x"}),
        )
        .await;
    assert!(
        !unblocked.contains("blocked while plan mode is active"),
        "after trusted exit_plan_mode approval, writes must be unblocked. Got: {unblocked}"
    );
}

#[tokio::test]
async fn write_guard_is_inactive_when_no_authoring_plan() {
    // Guard must be inactive (fail open) when there's no authoring plan,
    // regardless of whether cloud is configured or unavailable.
    for (label, with_cloud, mock_plan) in [
        ("no plan on cloud", true, true),
        ("no cloud binding", false, false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.txt");

        let executor = if with_cloud {
            let server = MockServer::start().await;
            if mock_plan {
                mock_no_authoring_plan(&server, "sess-1").await;
            }
            ToolExecutor::new(temp.path().to_path_buf())
                .with_active_session_id("sess-1")
                .with_cloud(server.uri(), "token")
        } else {
            ToolExecutor::new(temp.path().to_path_buf()).with_active_session_id("sess-1")
        };

        let result = executor
            .execute(
                "write_file",
                &json!({"path": target.to_string_lossy(), "content": "x"}),
            )
            .await;

        assert!(
            !result.contains("blocked while plan mode is active"),
            "[{label}] guard must be inactive. Got: {result}"
        );
    }
}

/// Typed task-control tools are the model-facing entry points. They must
/// dispatch to the background shell handlers instead of falling through to
/// unknown-tool handling. Verified here with the cheapest possible smoke: an
/// unwired CLI executor returns a known fail-fast string for each
/// action (the BackgroundTaskRegistry is wired only inside the TUI
/// REPL).
#[tokio::test]
async fn typed_background_task_tools_dispatch_through_executor() {
    let executor = test_executor();
    let result = executor
        .execute(
            "task_output",
            &json!({"task_id": "bg-shell-1", "block": false}),
        )
        .await;
    assert!(
        (result.contains("background") || result.contains("interactive REPL"))
            && !result.contains("Unknown tool"),
        "task_output should reach the registry-unwired fail-fast path. Got: {result}"
    );

    let result = executor
        .execute("task_stop", &json!({"task_id": "bg-shell-1"}))
        .await;
    assert!(
        (result.contains("background")
            || result.contains("interactive REPL")
            || result.contains("Nothing to stop"))
            && !result.contains("Unknown tool"),
        "task_stop should reach the registry-unwired fail-fast path. Got: {result}"
    );

    let result = executor.execute("task_list", &json!({})).await;
    assert!(
        (result.contains("background") || result.contains("interactive REPL"))
            && !result.contains("Unknown tool"),
        "task_list should reach the registry-unwired fail-fast path. Got: {result}"
    );
}

/// The `agent` tool's action enum must NOT advertise "delegate" to
/// the model. Schema-level removal is the strongest signal — the
/// model cannot even shape-validly emit a call the runtime would
/// silently no-op.
#[test]
fn agent_schema_enum_does_not_advertise_delegate_action() {
    let schemas = astra_tools::schemas::all_tool_schemas();
    let agent = schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(serde_json::Value::as_str)
                == Some("agent")
        })
        .expect("agent schema must exist");
    let actions = agent
        .get("function")
        .and_then(|f| f.get("parameters"))
        .and_then(|p| p.get("properties"))
        .and_then(|p| p.get("action"))
        .and_then(|a| a.get("enum"))
        .and_then(serde_json::Value::as_array)
        .expect("agent.action must declare an enum")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        !actions.contains(&"delegate"),
        "agent.action must NOT include 'delegate' — the action was \
         dead code (no delegation engine wired) and silently no-op'd. \
         Use spawn/get_result instead. Got actions: {actions:?}"
    );
    // Spawn must still be there — it's the working alternative.
    assert!(
        actions.contains(&"spawn"),
        "agent.action must still include 'spawn' (the working path the \
         delegate-error message redirects to). Got: {actions:?}"
    );
}

#[tokio::test]
async fn execute_reflect_returns_placeholder() {
    let executor = test_executor();
    let result = executor
        .execute(
            "reflect",
            &json!({
                "topic": "execution",
                "facet": "errors",
                "depth": "forensic",
                "horizon": "cross_session",
                "source_policy": "cloud_only",
                "include_context": true,
                "question": "why did the command fail?",
                "last_n": -10
            }),
        )
        .await;
    let value: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(value["status"], "reflect_requires_session");
    assert_eq!(value["topic"], "execution");
    assert_eq!(value["facet"], "errors");
    assert_eq!(value["depth"], "forensic");
    assert_eq!(value["horizon"], "cross_session");
    assert_eq!(value["source_policy"], "cloud_only");
    assert_eq!(value["include_context"], true);
    assert_eq!(value["question"], "why did the command fail?");
    assert_eq!(value["last_n"], 1);
    assert!(
        !result.contains("focus"),
        "removed focus parameter should not appear in placeholder: {result}"
    );
}

#[tokio::test]
async fn execute_reflect_uses_local_surface_with_session() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let session_id = "executor-reflect-session";
    let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
    ws.turn_count = 3;
    ws.last_context_trace = Some(ContextTraceSignal {
        turn_id: "turn-3".to_string(),
        captured_at: Some(Utc::now().to_rfc3339()),
        tool_surface: None,
        memory: None,
        history: None,
        budget: Some(
            astra_services::session_workspace::ContextTraceBudgetSignal {
                max_tokens: 10000,
                total_used: 8500,
                budget_pressure: 0.85,
                compression_triggered: false,
            },
        ),
        timing: None,
        explanations: vec![],
    });
    session_workspace::write_workspace(&ws).unwrap();

    session_journal::JournalWriter::new(session_id)
        .unwrap()
        .append(&JournalEvent {
            event_type: JournalEventType::TurnError,
            ts: Utc::now().to_rfc3339(),
            session_id: Some(session_id.to_string()),
            turn: Some(3),
            agentic_step: None,
            model: Some("gpt-5.4".to_string()),
            user_input: Some("debug".to_string()),
            assistant_output: None,
            tool_count: Some(1),
            tokens_in: Some(10),
            tokens_out: Some(20),
            duration_ms: Some(100),
            error: Some("bash failed".to_string()),
            config_key: None,
            config_value: None,
            turns_compacted: None,
            facts_stored: None,
            visible_tools: Some(vec!["bash".to_string()]),
            selected_skills: None,
            tools_used: Some(vec!["bash".to_string()]),
            tool_calls: Some(vec![session_journal::ToolCallRecord {
                name: "bash".to_string(),
                ok: false,
                ms: 100,
                error: Some("command failed".to_string()),
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            }]),
            budget_used: Some(8500),
            budget_pressure: Some(0.85),
            stall_type: None,
            metadata: None,
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            memoria_ms: None,
            session_lineage: None,
            coordination: None,
            edge_policy: None,
            context_assembly_trace: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            round: None,
            tool_calls_returned: None,
            offset_ms: None,
            llm_rounds: None,
            total_llm_ms: None,
            total_tool_ms: None,
            parent_event_id: None,
            git_head: None,
            git_branch: None,
        })
        .unwrap();

    let executor = ToolExecutor::new(temp.path().to_path_buf()).with_active_session_id(session_id);
    let result = executor
        .execute(
            "reflect",
            &json!({
                "topic": "execution",
                "facet": "trace",
                "depth": "forensic",
                "horizon": "cross_session",
                "source_policy": "cloud_only",
                "include_context": true,
                "last_n": 250
            }),
        )
        .await;
    let value: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(value["session_id"], session_id);
    assert_eq!(value["topic"], "execution");
    assert_eq!(value["facet"], "trace");
    assert_eq!(value["depth"], "forensic");
    assert_eq!(value["horizon"], "cross_session");
    assert_eq!(value["source_policy"], "cloud_only");
    assert_eq!(value["include_context"], true);
    assert_eq!(value["analysis_view"], "execution_trace");
    assert!(value.get("last_n").is_none());
    assert!(value.get("reflection_context").is_none());
    assert!(value.get("recent_turns").is_none());
    assert_eq!(value["data_coverage"]["overall"], "partial");
    assert_eq!(value["data_coverage"]["source"], "local_session_artifacts");
    assert_eq!(value["data_coverage"]["events"], 2);
    assert_eq!(
        value["data_coverage"]["providers"]["local_journal"]["status"],
        "fresh"
    );
    assert_eq!(
        value["data_coverage"]["providers"]["cloud_events"]["status"],
        "unavailable"
    );
    assert_eq!(
        value["data_coverage"]["providers"]["visible_context"]["status"],
        "partial"
    );
    let warnings = value["data_coverage"]["warnings"]
        .as_array()
        .expect("coverage warnings");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("cross_session"))),
        "cross-session local reflect requests must report partial coverage: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("cloud_only"))),
        "cloud-only local reflect requests must report partial coverage: {warnings:?}"
    );
    let observations = value["observations"]
        .as_array()
        .expect("observation records");
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0]["topic"], "execution");
    assert_eq!(observations[0]["facet"], "trace");
    assert_eq!(observations[0]["severity"], "warning");
    assert!(
        observations[0]["ref_id"]
            .as_str()
            .is_some_and(|ref_id| ref_id.starts_with("urn:astra:observation:local:reflect:")),
        "{value}"
    );
    let evidence = value["evidence"].as_array().expect("evidence");
    assert_eq!(evidence.len(), 1);
    assert!(
        evidence
            .iter()
            .all(|item| item["source"] == "local_journal"),
        "{evidence:?}"
    );
    assert!(
        evidence.iter().all(|item| item["ref_id"]
            .as_str()
            .is_some_and(|ref_id| ref_id.starts_with("urn:astra:event:local:"))),
        "{value}"
    );
    assert!(
        evidence.iter().any(|item| item["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("bash failed"))),
        "{value}"
    );
    let graph_nodes = value["graph_slice"]["nodes"]
        .as_array()
        .expect("graph nodes");
    assert_eq!(graph_nodes.len(), 2);
    assert!(
        graph_nodes
            .iter()
            .any(|node| node["layer"] == "observation"),
        "{graph_nodes:?}"
    );
    assert!(
        graph_nodes.iter().any(|node| node["layer"] == "runtime"),
        "{graph_nodes:?}"
    );
    assert_eq!(
        value["graph_slice"]["edges"]
            .as_array()
            .expect("graph edges")
            .len(),
        1
    );
    assert_eq!(value["data_coverage"]["events"], 2);
    // error information now lives in observations, not a separate overview.error_count field
    assert!(
        value["observations"]
            .as_array()
            .is_some_and(|obs| !obs.is_empty()),
        "expected at least one observation"
    );
}

#[test]
fn budget_pressure_defaults_to_zero() {
    let executor = test_executor();
    assert_eq!(executor.get_budget_pressure(), 0.0);
}

#[test]
fn budget_pressure_set_and_get() {
    let executor = test_executor();
    executor.set_budget_pressure(0.6);
    assert!((executor.get_budget_pressure() - 0.6).abs() < 1e-10);
}

#[test]
fn budget_pressure_clamps_to_range() {
    let executor = test_executor();
    executor.set_budget_pressure(1.5);
    assert_eq!(executor.get_budget_pressure(), 1.0);
    executor.set_budget_pressure(-0.5);
    assert_eq!(executor.get_budget_pressure(), 0.0);
}

// ── truncate_output ─────────────────────────────────────────────────────

#[test]
fn truncate_output_ascii_no_change() {
    let input = "hello world".to_string();
    let result = truncate_output(input.clone(), 100);
    assert_eq!(result, input);
}

#[test]
fn truncate_output_ascii_truncates() {
    let input = "hello world".to_string();
    let result = truncate_output(input, 5);
    assert!(result.starts_with("hello"));
    assert!(result.contains("[truncated]"));
}

#[test]
fn truncate_output_utf8_boundary_no_panic() {
    // 🔥 is 4 bytes, "ab🔥cd" = 2+4+2 = 8 bytes
    let input = "ab🔥cd".to_string();
    // Truncate at byte 3 — inside the 🔥 (bytes 2..5)
    let result = truncate_output(input, 3);
    // Should truncate at char boundary (byte 2, before 🔥)
    assert!(result.starts_with("ab"), "got: {result}");
    assert!(result.contains("[truncated]"));
}

#[test]
fn truncate_output_cjk_boundary_no_panic() {
    // Chinese chars are 3 bytes each
    let input = "你好世界".to_string(); // 12 bytes
    let result = truncate_output(input, 7); // Between 2nd and 3rd char
    assert!(result.contains("[truncated]"));
    // Should not panic — regression for char boundary issue
}
