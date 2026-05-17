use super::*;
use astra_services::session_journal::{self, JournalDirGuard, JournalEvent, JournalEventType};
use astra_services::session_workspace::{self, ContextTraceSignal, WorkspaceMetadata};
use chrono::Utc;
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
        result.contains("'nonexistent_tool'"),
        "error must quote the rejected tool name — got: {result}"
    );
    assert!(
        result.contains("not available"),
        "error must use the canonical 'not available' phrasing so dispatchers can pattern-match — got: {result}"
    );
}

/// Standalone `delegate` tool (the engine-managed delegation flow).
/// In server mode the runtime intercepts this call upstream in
/// `agentic_delegate_interception.rs` and runs real sub-agents — the
/// executor placeholder is only seen if interception was bypassed.
/// CLI mode wires no engine, but the tool name is reserved for
/// server-mode parity. Keep the deferred-acknowledgement contract
/// here; the broken path was the OTHER one (agent action='delegate'),
/// which is now removed entirely.
#[tokio::test]
async fn execute_delegate_tool_returns_deferred_acknowledgment_for_interception_fallback() {
    let executor = test_executor();
    let result = executor.execute("delegate", &json!({})).await;
    assert!(
        result.contains("Delegation request acknowledged"),
        "got: {result}"
    );
}

/// REGRESSION: the consolidated `agent` tool MUST NOT accept
/// `action='delegate'`. The CLI never wires a delegation engine, and
/// `agentic_delegate_interception` only intercepts calls whose tool
/// NAME is "delegate" — it ignores `agent(action='delegate')`. So the
/// old executor branch returned a "Delegation request acknowledged"
/// string while spawning nothing, and the model believed it had queued
/// real sub-agents. Bug observed: session f3c4b457-... shipped 5 fake
/// "Task done Delegating" rows in 0 ms each.
///
/// The fix is twofold: (1) the schema enum drops "delegate" so the
/// model can't pick it, and (2) defence-in-depth — the executor
/// rejects it as an unknown action with an actionable redirect.
#[tokio::test]
async fn agent_action_delegate_is_rejected_with_redirect_to_spawn() {
    let executor = test_executor();
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
        "agent.delegate must return an Error: prefix so the TUI renders \
         it as a failure (red banner), not as a normal tool result. Got: {result}"
    );
    assert!(
        result.contains("spawn"),
        "agent.delegate's error must name `agent.spawn` as the \
         alternative — without that, the model has no path to recovery. \
         Got: {result}"
    );
    assert!(
        !result.contains("Delegation request acknowledged"),
        "the old fake-success placeholder must be gone — its presence \
         is what tricked the model into believing 5 sub-agents were \
         queued when none had spawned. Got: {result}"
    );
    // End-to-end UX assertion: the Error: prefix must classify through
    // tool_result_semantics::is_tool_error → cloud_tool_result_status_label
    // → "error", so the TUI renders the red `•` failure banner instead
    // of the green success banner. This is the load-bearing wire that
    // makes the failure visible to the human.
    assert!(
        astra_turn_core::tool_result_semantics::is_tool_error(&result),
        "the new error must be classified as an error by is_tool_error \
         (drives TUI red banner / Failed label). Got: {result}"
    );
    assert_eq!(
        astra_turn_core::tool_result_semantics::cloud_tool_result_status_label(&result),
        "error",
        "the new error must produce status='error' for cloud reporting; \
         status='success' would re-poison the model's belief that the \
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
    let executor = test_executor();
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

/// REGRESSION: after the Phase 1 split, `task.background_shell` and
/// the other 3 background actions live on the new `agent_job` tool.
/// Calling them on `task` is a sign the model is on a stale schema or
/// hallucinating a path that no longer exists. The executor must
/// surface this as an Error: with a redirect — same shape as the
/// `agent.delegate` rejection — so the model self-corrects on the
/// next turn instead of silently failing.
#[tokio::test]
async fn task_background_actions_are_rejected_with_redirect_to_agent_job() {
    let executor = test_executor();
    for (action, redirect_action) in &[
        ("background_shell", "shell"),
        ("background_agent", "agent"),
        ("output", "output"),
        ("kill", "kill"),
    ] {
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
            result.contains("agent_job"),
            "task.{action} error must name `agent_job` as the new home — \
             without that, the model has no path to recovery. Got: {result}"
        );
        assert!(
            result.contains(&format!("agent_job(action='{redirect_action}')"))
                || result.contains(redirect_action),
            "task.{action} error must point at the specific replacement \
             action `agent_job(action='{redirect_action}')`. Got: {result}"
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
async fn exit_plan_mode_accepts_plan_alias_and_explicit_approved_skips_overlay() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(header("authorization", "Bearer token"))
        .and(query_param("session_id", "sess-1"))
        .and(query_param("phase", "planning"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": [
                { "plan_id": "plan-2", "goal": "Ship auth" }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/plans/plan-2/exit-plan-mode"))
        .and(header("authorization", "Bearer token"))
        .and(body_json(json!({
            "approved": true,
            "plan_md": "1. Ship auth"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_id": "plan-2",
            "phase": "refining"
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");

    // Explicit `approved: true` is the headless / harness escape hatch:
    // it bypasses the interactive Approve / Keep-planning overlay and
    // commits the plan directly. Interactive callers leave `approved`
    // out so the TUI can surface the 4-option dialog.
    let result = executor
        .execute(
            "exit_plan_mode",
            &json!({"plan": "1. Ship auth", "approved": true}),
        )
        .await;

    assert!(
        result.starts_with("Exited plan mode."),
        "exit_plan_mode should accept schema-native `plan` and approve when `approved=true` is explicit. Got: {result}"
    );
    assert!(
        result.contains("plan-2"),
        "result should mention the resolved plan id. Got: {result}"
    );
}

#[tokio::test]
async fn plan_mode_write_guard_cache_is_invalidated_when_session_changes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-plan"))
        .and(query_param("phase", "planning"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": [{ "plan_id": "plan-1", "goal": "Ship auth" }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-normal"))
        .and(query_param("phase", "planning"))
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

#[tokio::test]
async fn exit_plan_mode_overlay_approve_auto_records_pending_mode_change() {
    // End-to-end of the Approve / Keep-planning dialog: model calls
    // exit_plan_mode without `approved`; the executor surfaces the
    // 4-option dialog through `plan_review_request_tx`; the test
    // auto-answers with PermissionMode::Auto so the executor commits
    // the plan and stages Auto in the pending-mode slot.
    use crate::chat_stream::PlanReviewDecision;
    use crate::permission_manager::PermissionMode;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-1"))
        .and(query_param("phase", "planning"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": [{ "plan_id": "plan-7", "goal": "Ship auth" }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/plans/plan-7/exit-plan-mode"))
        .and(body_json(json!({
            "approved": true,
            "plan_md": "1. Ship auth"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_id": "plan-7",
            "phase": "refining"
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");

    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::chat_stream::PlanReviewRequest>();
    executor.set_plan_review_request_tx(Some(tx));

    let overlay_task = tokio::spawn(async move {
        let request = rx.recv().await.expect("overlay request");
        let _ = request.response_tx.send(PlanReviewDecision::Approve {
            mode: PermissionMode::Auto,
        });
    });

    let result = executor
        .execute("exit_plan_mode", &json!({"plan": "1. Ship auth"}))
        .await;
    overlay_task.await.unwrap();

    assert!(
        result.starts_with("Exited plan mode."),
        "approve path should commit the plan. Got: {result}"
    );
    assert!(
        result.contains("auto"),
        "tool result should advertise the next-turn mode so the model knows the runtime is now permissive. Got: {result}"
    );
    assert_eq!(
        executor.take_pending_permission_mode_change(),
        Some(crate::permission_manager::PermissionMode::Auto),
        "host must see Auto staged in the pending slot to apply on the next turn"
    );
}

// ── Plan-mode systemic invariants (Step 4) ────────────────────────────
//
// These tests pin the invariants from the cross-deployment audit. They
// were written first and stay red until each piece of the refactor
// lands. See `project_plan_mode_schema_truth` memory for the audit
// table mapping I1..I9 to deployment shapes.

#[tokio::test]
async fn enter_plan_mode_falls_back_to_local_when_cloud_unavailable() {
    // Invariant I1 + I2: pressing Shift+Tab → Plan must succeed
    // even with no cloud reachable. Today `enter_plan_mode_remote`
    // hard-requires `cloud_token`; offline / unauthenticated CLI
    // sessions silently lose plan mode. After the refactor an
    // unconfigured cloud must route to the same overlay-less local
    // entry: stage `PermissionMode::Plan` in the pending slot, no
    // network calls, no `Error:` prefix.
    //
    // RED until Step 4-3 lands.
    let temp = tempfile::tempdir().unwrap();
    let executor =
        ToolExecutor::new(temp.path().to_path_buf()).with_active_session_id("sess-offline");
    // Deliberately no `with_cloud(...)` — simulates Shift+Tab in a
    // detached CLI run.

    let result = executor
        .execute("enter_plan_mode", &json!({"goal": "Investigate auth"}))
        .await;

    assert!(
        !result.starts_with("Error:"),
        "enter_plan_mode must not error out without a cloud token; \
         it should fall back to a local plan-mode pivot. Got: {result}"
    );
    assert_eq!(
        executor.take_pending_permission_mode_change(),
        Some(crate::permission_manager::PermissionMode::Plan),
        "entering plan mode must stage Plan on the pending slot \
         so the host applies it on the next turn"
    );
}

#[tokio::test]
async fn enter_plan_mode_stages_perm_mode_change_to_plan_even_on_cloud_path() {
    // Invariant I6: a single source of truth for "am I in plan mode".
    // Today the cloud path (`enter_plan_mode_remote` with a real
    // server) flips the cloud `plans` row but leaves `perm_manager`
    // untouched — so `perm_manager.mode()` and the cloud row
    // disagree. The host then has no signal to update the schema
    // for the next turn. This test pins the contract that whichever
    // path runs, the perm-mode change always lands in the slot.
    //
    // RED until Step 4-3 wires the slot from the cloud branch too.
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

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-cloud")
        .with_cloud(server.uri(), "token");

    let result = executor
        .execute("enter_plan_mode", &json!({"goal": "Ship auth"}))
        .await;

    assert!(
        !result.starts_with("Error:"),
        "cloud path must succeed when the server returns a plan_id. Got: {result}"
    );
    assert_eq!(
        executor.take_pending_permission_mode_change(),
        Some(crate::permission_manager::PermissionMode::Plan),
        "cloud path must stage Plan on the pending slot too — single \
         source of truth invariant"
    );
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
    use crate::chat_stream::PlanReviewDecision;
    use crate::permission_manager::PermissionMode;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-no-cloud"))
        .and(query_param("phase", "planning"))
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
        tokio::sync::mpsc::unbounded_channel::<crate::chat_stream::PlanReviewRequest>();
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
        Some(crate::permission_manager::PermissionMode::Plan),
        "enter must stage Plan"
    );

    // Step 2-4: exit through overlay, choose Auto
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::chat_stream::PlanReviewRequest>();
    executor.set_plan_review_request_tx(Some(tx));

    let overlay_task = tokio::spawn(async move {
        let request = rx.recv().await.expect("overlay request");
        let _ = request
            .response_tx
            .send(crate::chat_stream::PlanReviewDecision::Approve {
                mode: crate::permission_manager::PermissionMode::Auto,
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
        Some(crate::permission_manager::PermissionMode::Auto),
        "exit must stage Auto"
    );
}

#[tokio::test]
async fn exit_plan_mode_overlay_keep_planning_leaves_plan_open() {
    // User chooses "Keep planning"; the plan must stay in `planning`
    // phase (server call body `approved: false`), no permission-mode
    // change should be staged, and the message must tell the model
    // to address feedback before re-calling.
    use crate::chat_stream::PlanReviewDecision;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-1"))
        .and(query_param("phase", "planning"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": [{ "plan_id": "plan-8", "goal": "Ship auth" }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/plans/plan-8/exit-plan-mode"))
        .and(body_json(json!({
            "approved": false,
            "plan_md": "1. Draft"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_id": "plan-8",
            "phase": "planning"
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");

    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::chat_stream::PlanReviewRequest>();
    executor.set_plan_review_request_tx(Some(tx));

    let overlay_task = tokio::spawn(async move {
        let request = rx.recv().await.expect("overlay request");
        let _ = request.response_tx.send(PlanReviewDecision::KeepPlanning);
    });

    let result = executor
        .execute("exit_plan_mode", &json!({"plan": "1. Draft"}))
        .await;
    overlay_task.await.unwrap();

    assert!(
        result.contains("left open"),
        "keep-planning path should signal the plan stays open. Got: {result}"
    );
    assert!(
        result.contains("feedback"),
        "result should tell the model to address feedback before re-calling. Got: {result}"
    );
    assert_eq!(
        executor.take_pending_permission_mode_change(),
        None,
        "no mode change should be staged when the user keeps planning"
    );
}

#[tokio::test]
async fn exit_plan_mode_shift_tab_path_works_without_cloud_plan_record() {
    // Regression for session d9b5119f: user pressed Shift+Tab to
    // enter plan mode (so `perm_manager.mode() == Plan`, but no
    // cloud `plans` row was created). The model authored a plan and
    // called `exit_plan_mode(plan="…")`. The previous implementation
    // hard-required a `phase=planning` cloud record and bailed with
    // "no active planning plan found for the current session" — the
    // agent was stuck in plan mode forever.
    //
    // After the dual-path refactor the same call must complete via
    // the local-only path: probe the cloud, find nothing, fall
    // through to the overlay + `pending_permission_mode_change` slot
    // exactly like the cloud path does, and never hit any
    // `/plans/*/exit-plan-mode` endpoint.
    use crate::chat_stream::PlanReviewDecision;
    use crate::permission_manager::PermissionMode;

    let server = MockServer::start().await;
    // Probe returns an empty plans array — that's the trigger for
    // the local fallback. We deliberately do NOT mount any
    // `POST /plans/*/exit-plan-mode` mock; if the implementation
    // were to call it the test would fail with an unexpected request.
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-shift-tab"))
        .and(query_param("phase", "planning"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"plans": []})))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-shift-tab")
        .with_cloud(server.uri(), "token");

    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::chat_stream::PlanReviewRequest>();
    executor.set_plan_review_request_tx(Some(tx));

    let overlay_task = tokio::spawn(async move {
        let request = rx.recv().await.expect("overlay request");
        let _ = request.response_tx.send(PlanReviewDecision::Approve {
            mode: PermissionMode::AcceptEdits,
        });
    });

    let result = executor
        .execute(
            "exit_plan_mode",
            &json!({"plan": "1. Read auth flow\n2. Add tests"}),
        )
        .await;
    overlay_task.await.unwrap();

    assert!(
        result.starts_with("Exited plan mode"),
        "Shift+Tab path must complete the exit even without a cloud plan record. Got: {result}"
    );
    assert!(
        !result.contains("no active planning plan"),
        "must not surface the cloud-lookup error any more. Got: {result}"
    );
    assert!(
        result.contains("edit"),
        "tool result should advertise the chosen mode. Got: {result}"
    );
    assert!(
        result.contains("1. Read auth flow"),
        "local path should echo the plan markdown so the next turn has it in context. Got: {result}"
    );
    assert_eq!(
        executor.take_pending_permission_mode_change(),
        Some(crate::permission_manager::PermissionMode::AcceptEdits),
        "host must see AcceptEdits staged in the pending slot for the next turn"
    );
}

#[tokio::test]
async fn exit_plan_mode_without_overlay_or_approved_returns_actionable_error() {
    // Without an `ask_user_request_tx` installed (headless / sub-run
    // context) and without an explicit `approved` field, the model
    // would otherwise hang forever waiting for a TUI dialog that will
    // never arrive. We instead return a clear error so the model can
    // re-call with `approved=true|false`. Pinning the message text
    // because both halves matter:
    //   - "requires an interactive TUI overlay" tells the human reader
    //     why the call failed in this environment;
    //   - "Re-call with `approved=true` or `approved=false`" gives the
    //     model a concrete recovery action so it doesn't spin.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(query_param("session_id", "sess-1"))
        .and(query_param("phase", "planning"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": [{ "plan_id": "plan-3", "goal": "Ship auth" }]
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");

    let result = executor
        .execute("exit_plan_mode", &json!({"plan": "1. Ship auth"}))
        .await;

    assert!(
        result.starts_with("Error:"),
        "expected an error result without overlay channel. Got: {result}"
    );
    assert!(
        result.contains("interactive TUI overlay"),
        "error should explain the missing overlay sink. Got: {result}"
    );
    assert!(
        result.contains("approved"),
        "error should suggest `approved=true|false` as a fallback. Got: {result}"
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

async fn mock_planning_plan_present(server: &MockServer, session_id: &str, plan_id: &str) {
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(header("authorization", "Bearer token"))
        .and(query_param("session_id", session_id))
        .and(query_param("phase", "planning"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plans": [
                { "plan_id": plan_id, "goal": "Ship auth" }
            ]
        })))
        .mount(server)
        .await;
}

async fn mock_no_planning_plan(server: &MockServer, session_id: &str) {
    Mock::given(method("GET"))
        .and(path("/plans"))
        .and(header("authorization", "Bearer token"))
        .and(query_param("session_id", session_id))
        .and(query_param("phase", "planning"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"plans": []})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn write_file_is_blocked_while_plan_mode_is_authoring() {
    let server = MockServer::start().await;
    mock_planning_plan_present(&server, "sess-1", "plan-9").await;

    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");
    let target = temp.path().join("ledger.txt");

    let result = executor
        .execute(
            "write_file",
            &json!({"path": target.to_string_lossy(), "content": "x"}),
        )
        .await;

    assert!(
        result.contains("blocked while plan mode is active"),
        "write_file must be blocked while a plan is being authored (parity with server). Got: {result}"
    );
    assert!(
        result.contains("exit_plan_mode"),
        "the error must point the model at exit_plan_mode as the escape hatch. Got: {result}"
    );
    assert!(
        !target.exists(),
        "the guard must short-circuit BEFORE any file is created on disk"
    );
}

#[tokio::test]
async fn bash_is_blocked_while_plan_mode_is_authoring() {
    let server = MockServer::start().await;
    mock_planning_plan_present(&server, "sess-1", "plan-9").await;

    let temp = tempfile::tempdir().unwrap();
    let canary = temp.path().join("canary.txt");
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");

    let result = executor
        .execute(
            "bash",
            &json!({"command": format!("touch {}", canary.display())}),
        )
        .await;

    assert!(
        result.contains("blocked while plan mode is active"),
        "bash must be blocked while a plan is being authored. Got: {result}"
    );
    assert!(
        !canary.exists(),
        "the bash guard must run before the shell, so no side effects leak"
    );
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

    // Approve exit.
    let exit = executor
        .execute("exit_plan_mode", &json!({"approved": true}))
        .await;
    assert!(
        exit.starts_with("Exited plan mode."),
        "exit_plan_mode(approved=true) must succeed. Got: {exit}"
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
        "after exit_plan_mode(approved=true), writes must be unblocked. Got: {unblocked}"
    );
}

#[tokio::test]
async fn write_guard_is_inactive_when_no_planning_plan_exists() {
    let server = MockServer::start().await;
    mock_no_planning_plan(&server, "sess-1").await;

    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("note.txt");
    let executor = ToolExecutor::new(temp.path().to_path_buf())
        .with_active_session_id("sess-1")
        .with_cloud(server.uri(), "token");

    let result = executor
        .execute(
            "write_file",
            &json!({"path": target.to_string_lossy(), "content": "x"}),
        )
        .await;

    assert!(
        !result.contains("blocked while plan mode is active"),
        "with no planning plan present, the guard must be inactive. Got: {result}"
    );
}

#[tokio::test]
async fn write_guard_is_inactive_without_cloud_binding() {
    // No cloud binding ⇒ guard cannot consult the plan store; it must
    // fail open so offline / unauthenticated CLI runs still work.
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("note.txt");
    let executor = ToolExecutor::new(temp.path().to_path_buf()).with_active_session_id("sess-1");

    let result = executor
        .execute(
            "write_file",
            &json!({"path": target.to_string_lossy(), "content": "x"}),
        )
        .await;

    assert!(
        !result.contains("blocked while plan mode is active"),
        "without cloud binding, the guard must fail open. Got: {result}"
    );
}

/// `agent_job` is the new entry point. It must dispatch the four
/// background actions to the same handlers that previously sat on
/// `task` — verified here with the cheapest possible smoke: an
/// unwired CLI executor returns a known fail-fast string for each
/// action (the BackgroundTaskRegistry is wired only inside the TUI
/// REPL — see `task_background_shell_fails_fast_when_unwired`).
#[tokio::test]
async fn agent_job_actions_dispatch_through_executor() {
    let executor = test_executor();
    // shell — needs `command`; without the registry wired we expect
    // the unwired-fast-fail path, not the missing-arg path.
    let result = executor
        .execute(
            "agent_job",
            &json!({"action": "shell", "command": "echo hi"}),
        )
        .await;
    assert!(
        result.contains("background_shell")
            || result.contains("interactive REPL")
            || result.contains("not available"),
        "agent_job.shell should reach the same fail-fast path that \
         task.background_shell used to hit (registry only wired inside \
         the TUI). Got: {result}"
    );

    // output — must reach the same handler that returns the unwired
    // message; both kill and output share the registry dependency.
    let result = executor
        .execute(
            "agent_job",
            &json!({"action": "kill", "task_id": "bg-shell-1"}),
        )
        .await;
    assert!(
        result.contains("background")
            || result.contains("interactive REPL")
            || result.contains("Nothing to kill"),
        "agent_job.kill should reach the registry-unwired fail-fast path. \
         Got: {result}"
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
    let result = executor.execute("reflect", &json!({"focus": "auto"})).await;
    assert!(result.contains("reflect_requires_session"), "got: {result}");
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
        tool_selection: None,
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
            tools_selected: Some(vec!["bash".to_string()]),
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
            selection_trace: None,
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
        .execute("reflect", &json!({"focus": "performance"}))
        .await;
    let value: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(value["session_id"], session_id);
    assert_eq!(value["focus"], "performance");
    // Liquid-reflection subsystem removed. `recent_turns` surfaces the
    // focused journal preview that downstream UIs read directly.
    let recent = value["recent_turns"].as_array().expect("recent_turns");
    assert!(!recent.is_empty(), "turn journal preview should be present");
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
