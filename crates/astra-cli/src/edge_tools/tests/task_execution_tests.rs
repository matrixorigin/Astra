//! Public executor journeys, independent of a TUI command-draining loop.
use super::*;
use astra_tools::tool_engine::ToolInvocationMetadata;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

struct TestWorkspace {
    root: tempfile::TempDir,
    project: std::path::PathBuf,
}

impl TestWorkspace {
    fn path(&self) -> &std::path::Path {
        &self.project
    }
}

fn fixture() -> (TestWorkspace, Arc<ToolExecutor>) {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let tasks = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        root.path().join("captures"),
    );
    let executor = Arc::new(ToolExecutor::new(&project).with_background_tasks(tasks));
    (TestWorkspace { root, project }, executor)
}

async fn call(
    executor: &ToolExecutor,
    name: &str,
    args: Value,
) -> astra_tools::execution_outcome::ToolExecutionOutcome {
    executor
        .execute_with_invocation_metadata_cancelable(
            name,
            &args,
            ToolInvocationMetadata::default(),
            None,
        )
        .await
}

fn output_chunk(outcome: &astra_tools::execution_outcome::ToolExecutionOutcome) -> String {
    serde_json::from_str::<Value>(&outcome.output)
        .ok()
        .and_then(|value| {
            value
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

async fn observe_challenge(
    executor: &ToolExecutor,
    id: &str,
    prompt: &str,
    mut revision: Option<u64>,
) -> (astra_tools::execution_outcome::ToolExecutionOutcome, String) {
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let mut args = json!({
                "task_id":id, "block":true, "wake_on_output":true, "timeout_ms":4000
            });
            if let Some(previous) = revision {
                args["after_output_revision"] = json!(previous);
            }
            let result = call(executor, "task_output", args).await;
            assert!(!result.is_error, "{result:?}");
            let output = output_chunk(&result);
            if let Some((_, rest)) = output.split_once(prompt)
                && let Some((code, _)) = rest.split_once('\n')
            {
                let code = code.trim_end_matches('\r');
                if code.len() == 16
                    && code
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return (result, code.to_owned());
                }
            }
            revision = Some(result.tool_result_fields.as_ref().unwrap()
                ["background_task_observation"]["output_revision"].as_u64().unwrap());
        }
    })
    .await
    .expect("complete challenge output")
}

async fn launch(executor: &ToolExecutor, command: &str, stdin: bool, timeout: f64) -> String {
    let outcome = call(
        executor,
        "bash",
        json!({"command":command,"stdin":stdin,"yield_time_ms":0,"timeout":timeout}),
    )
    .await;
    assert!(!outcome.is_error, "{outcome:?}");
    outcome.tool_result_fields.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .into()
}

async fn stdin_ready(executor: &ToolExecutor, id: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let task = executor.background_tasks.get(id).unwrap();
            assert!(
                !task.status().is_terminal(),
                "task ended before stdin opened"
            );
            if task.stdin_available() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stdin readiness");
}

async fn terminal(
    executor: &ToolExecutor,
    id: &str,
) -> Arc<astra_tools::execution_outcome::ToolExecutionOutcome> {
    tokio::time::timeout(
        Duration::from_secs(10),
        executor.background_tasks.subscription(id).unwrap().wait(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn delivered_terminal_output_clears_known_failure_attention() {
    let (_root, executor) = fixture();
    let id = launch(
        &executor,
        "printf 'failed check\\n' >&2; exit 1",
        false,
        10.0,
    )
    .await;
    terminal(&executor, &id).await;
    let before = executor.background_tasks.get(&id).unwrap();
    assert!(before.terminal_unobserved());

    let result = call(&executor, "task_output", json!({"task_id": id})).await;
    assert!(result.output.contains("failed check"), "{result:?}");
    assert!(
        executor
            .background_tasks
            .get(&id)
            .unwrap()
            .terminal_unobserved()
    );
    executor.acknowledge_delivered_task_result("task_output", result.tool_result_fields.as_ref());
    assert!(
        !executor
            .background_tasks
            .get(&id)
            .unwrap()
            .terminal_unobserved()
    );
    let mut source = executor.background_tasks.clone();
    let projections = source.export_shell_task_projections();
    let restored_dir = tempfile::tempdir().unwrap();
    let mut restored = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        restored_dir.path().to_path_buf(),
    );
    restored
        .restore_shell_task_projections(projections)
        .unwrap();
    let history = restored.get(&id).expect("failed task remains inspectable");
    assert!(!history.terminal_unobserved());
    assert!(!history.outcome_unsettled());
}

#[tokio::test]
async fn partial_output_reports_its_window_and_acknowledges_the_terminal_fact() {
    let (_root, executor) = fixture();
    let id = launch(
        &executor,
        "printf '%01024d' 0; printf 'compiler error at end\\n' >&2; exit 1",
        false,
        10.0,
    )
    .await;
    terminal(&executor, &id).await;
    let first = call(
        &executor,
        "task_output",
        json!({"task_id": id, "offset": 0, "max_bytes": 32}),
    )
    .await;
    assert!(first.output.contains("only part"), "{first:?}");
    let mut timeout = first.tool_result_fields.clone().unwrap();
    timeout.insert("wait_result".into(), json!("timeout"));
    executor.acknowledge_delivered_task_result("task_output", Some(&timeout));
    let mut different_task = first.tool_result_fields.clone().unwrap();
    different_task.get_mut("task").unwrap()["task_id"] = json!("another-task");
    executor.acknowledge_delivered_task_result("task_output", Some(&different_task));
    assert!(
        executor
            .background_tasks
            .get(&id)
            .unwrap()
            .terminal_unobserved()
    );
    executor.acknowledge_delivered_task_result("task_output", first.tool_result_fields.as_ref());
    assert!(
        !executor
            .background_tasks
            .get(&id)
            .unwrap()
            .terminal_unobserved()
    );
    let last = call(&executor, "task_output", json!({"task_id": id})).await;
    assert!(last.output.contains("compiler error at end"), "{last:?}");
    executor.acknowledge_delivered_task_result("task_output", last.tool_result_fields.as_ref());
    assert!(
        !executor
            .background_tasks
            .get(&id)
            .unwrap()
            .terminal_unobserved()
    );
}

#[tokio::test]
async fn interrupted_task_can_acknowledge_all_retained_output_without_claiming_complete_execution()
{
    let (_root, executor) = fixture();
    let id = launch(&executor, "printf ready; sleep 60", false, 60.0).await;
    executor.background_tasks.clone().kill(&id).unwrap();
    terminal(&executor, &id).await;
    let result = call(
        &executor,
        "task_output",
        json!({"task_id": id, "offset": 0, "max_bytes": 8192}),
    )
    .await;
    let snapshot: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(snapshot["output_complete"], false);
    executor.acknowledge_delivered_task_result("task_output", result.tool_result_fields.as_ref());
    assert!(
        !executor
            .background_tasks
            .get(&id)
            .unwrap()
            .terminal_unobserved()
    );
}

#[tokio::test]
async fn reading_failed_task_does_not_erase_its_pending_notification() {
    let (_root, executor) = fixture();
    let _consumer =
        executor
            .background_tasks
            .clone()
            .with_active_work_registry(std::sync::Arc::new(
                astra_core::work_unit::ActiveWorkRegistry::default(),
            ));
    let id = launch(
        &executor,
        "printf 'compile failed\\n' >&2; exit 1",
        false,
        10.0,
    )
    .await;
    terminal(&executor, &id).await;
    let mut registry = executor.background_tasks.clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            registry.drain_join_set();
            if registry
                .get(&id)
                .is_some_and(|task| task.ended_at_ms.is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal event must be queued");
    let result = call(&executor, "task_output", json!({"task_id": id})).await;
    executor.acknowledge_delivered_task_result("task_output", result.tool_result_fields.as_ref());
    let events = registry.poll_completions();
    let failures: Vec<_> = events.iter().filter(|event| matches!(event,
        crate::edge_tools::background_tasks::BgTaskEvent::Failed { id: task, .. } if task == &id
    )).collect();
    assert_eq!(failures.len(), 1, "{events:?}");
    assert!(registry.poll_completions().is_empty());
}

#[tokio::test]
async fn delivered_unknowns_do_not_fill_a_registry_without_notification_consumer() {
    let (root, executor) = fixture();
    let mut registry = executor.background_tasks.clone();
    for _ in 0..100 {
        let mut task = registry
            .start_shell(
                "foreground operation",
                (root.path().join("out"), root.path().join("err"), None),
                false,
                false,
                None,
                |_, _| async {
                    let mut result =
                        astra_tools::execution_outcome::ToolExecutionOutcome::error_with_evidence(
                            "outcome unknown".into(),
                            astra_core::ToolFailureEvidence::from_error_kind(
                                astra_core::ErrorKind::ToolOutcomeUnknown,
                            ),
                        );
                    result
                        .tool_result_fields
                        .as_mut()
                        .unwrap()
                        .insert("cleanup_authoritative".into(), json!(true));
                    result
                },
            )
            .expect("delivered notifications must not exhaust a headless registry");
        let result = task.wait().await.unwrap();
        registry.abandon_foreground(&task.id);
        executor.acknowledge_delivered_task_result("bash", result.tool_result_fields.as_ref());
    }
    assert!(registry.poll_completions().is_empty());
}

#[tokio::test]
async fn real_long_shell_failure_notification_includes_the_error_tail() {
    let (_root, executor) = fixture();
    let id = launch(
        &executor,
        "printf '%020000d' 0 >&2; printf '\\nunique compiler error at end\\n' >&2; exit 1",
        false,
        10.0,
    )
    .await;
    terminal(&executor, &id).await;
    let mut registry = executor.background_tasks.clone();
    let events = registry.poll_completions();
    let error = events
        .iter()
        .find_map(|event| match event {
            crate::edge_tools::background_tasks::BgTaskEvent::Failed {
                id: task, error, ..
            } if task == &id => Some(error),
            _ => None,
        })
        .expect("real shell failure notification");
    assert!(error.contains("unique compiler error at end"), "{error}");
    assert!(error.contains("truncated"), "{error}");
    assert!(error.contains("task_output"), "{error}");
}

#[tokio::test]
async fn restored_failure_is_observed_when_terminal_output_is_inspected() {
    use astra_services::session_workspace::{
        BackgroundShellTaskProjection, BackgroundShellTerminalFacts,
    };
    let (root, _) = fixture();
    let stdout = root.path().join("saved-stdout");
    let stderr = root.path().join("saved-stderr");
    std::fs::write(&stdout, "").unwrap();
    std::fs::write(&stderr, "compiler error\n").unwrap();
    let mut registry = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        root.root.path().join("restore"),
    )
    .with_active_work_registry(std::sync::Arc::new(
        astra_core::work_unit::ActiveWorkRegistry::default(),
    ));
    registry
        .restore_shell_task_projection(BackgroundShellTaskProjection {
            id: "restored-failure".into(),
            status: "failed".into(),
            title: "compile".into(),
            started_at_ms: 1,
            ended_at_ms: Some(2),
            stdout_path: stdout.display().to_string(),
            stderr_path: stderr.display().to_string(),
            exit_code: Some(1),
            terminal_reason: Some("compiler error".into()),
            terminal_facts: Some(BackgroundShellTerminalFacts {
                unobserved: true,
                outcome_unknown: false,
                cleanup_unconfirmed: false,
            }),
        })
        .unwrap();
    let executor = ToolExecutor::new(root.path()).with_background_tasks(registry.clone());
    let result = call(
        &executor,
        "task_output",
        json!({"task_id":"restored-failure"}),
    )
    .await;
    assert!(result.output.contains("compiler error"), "{result:?}");
    executor.acknowledge_delivered_task_result("task_output", result.tool_result_fields.as_ref());
    assert!(
        !registry
            .get("restored-failure")
            .unwrap()
            .terminal_unobserved()
    );
    assert!(registry.poll_completions().iter().any(|event| matches!(event, crate::edge_tools::background_tasks::BgTaskEvent::Failed { id, .. } if id == "restored-failure")));
    let persisted = registry.export_shell_task_projections();
    let mut reopened = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        root.root.path().join("reopen"),
    );
    reopened.restore_shell_task_projections(persisted).unwrap();
    assert!(
        !reopened
            .get("restored-failure")
            .unwrap()
            .terminal_unobserved()
    );
}

#[tokio::test]
async fn restored_failure_with_missing_output_delivers_available_facts_once() {
    use astra_services::session_workspace::{
        BackgroundShellTaskProjection, BackgroundShellTerminalFacts,
    };
    let (root, _) = fixture();
    let mut registry = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        root.root.path().join("restore-missing"),
    );
    registry
        .restore_shell_task_projection(BackgroundShellTaskProjection {
            id: "restored-missing".into(),
            status: "failed".into(),
            title: "compile".into(),
            started_at_ms: 1,
            ended_at_ms: Some(2),
            stdout_path: String::new(),
            stderr_path: String::new(),
            exit_code: Some(1),
            terminal_reason: Some("compiler error".into()),
            terminal_facts: Some(BackgroundShellTerminalFacts {
                unobserved: true,
                outcome_unknown: false,
                cleanup_unconfirmed: false,
            }),
        })
        .unwrap();
    let executor = ToolExecutor::new(root.path()).with_background_tasks(registry.clone());
    let result = call(
        &executor,
        "task_output",
        json!({"task_id":"restored-missing"}),
    )
    .await;
    let output: Value = serde_json::from_str(&result.output).unwrap();
    assert!(
        matches!(
            output["status"].as_str(),
            Some("output_missing" | "output_unavailable")
        ),
        "{output}"
    );
    assert_eq!(output["task"]["status"], "failed");
    assert_eq!(output["task"]["terminal_reason"], "compiler error");
    assert!(
        output["output_notice"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    executor.acknowledge_delivered_task_result("task_output", result.tool_result_fields.as_ref());
    assert!(
        !registry
            .get("restored-missing")
            .unwrap()
            .terminal_unobserved()
    );
    let persisted = registry.export_shell_task_projections();
    let mut reopened = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        root.root.path().join("reopen-missing"),
    );
    reopened.restore_shell_task_projections(persisted).unwrap();
    assert!(
        !reopened
            .get("restored-missing")
            .unwrap()
            .terminal_unobserved()
    );
}

async fn recorded(executor: &ToolExecutor, id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if executor
                .background_tasks
                .get(id)
                .is_some_and(|task| task.ended_at_ms.is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal completion recorded");
}

#[tokio::test]
async fn foreground_completions_do_not_evict_background_output() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "printf background-result", false, 5.0).await;
    terminal(&executor, &id).await;
    recorded(&executor, &id).await;
    // Exceed the production retention limit with real, truncated foreground
    // results; a background task must remain inspectable.
    for _ in 0..34 {
        let result = call(
            &executor,
            "bash",
            json!({"command":"printf foreground; seq 1 20000", "timeout":5}),
        )
        .await;
        assert!(
            !result.is_error && result.output.contains("foreground"),
            "{result:?}"
        );
    }
    let result = call(&executor, "task_output", json!({"task_id":id})).await;
    assert!(result.output.contains("background-result"), "{result:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_the_turn_stops_its_foreground_and_detached_processes() {
    for detached in [false, true] {
        let (workspace, executor) = fixture();
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker = {
            let (executor, token) = (executor.clone(), cancel.clone());
            tokio::spawn(async move {
                let mut args =
                    json!({"command":"printf '%s\\n' $$ > pid; exec sleep 30", "timeout":35});
                if detached {
                    args["yield_time_ms"] = json!(0);
                }
                executor
                    .execute_with_invocation_metadata_cancelable(
                        "bash",
                        &args,
                        ToolInvocationMetadata::default(),
                        Some(&token),
                    )
                    .await
            })
        };
        let pid: i32 = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(workspace.path().join("pid"))
                    && let Ok(pid) = text.trim().parse()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the real shell must have started");
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap();
        if detached {
            let id = result.tool_result_fields.as_ref().unwrap()["task_id"]
                .as_str()
                .unwrap();
            assert!(terminal(&executor, id).await.is_error);
        } else {
            assert!(result.is_error, "{result:?}");
        }
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "process {pid} survived cancellation"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[tokio::test]
async fn dropped_foreground_wait_does_not_leave_an_invisible_capture() {
    let (root, executor) = fixture();
    let mut registry = executor.background_tasks.clone();
    for index in 0..8 {
        let marker = root.path().join(format!("started-{index}"));
        let command = format!("printf started > started-{index}; sleep 30");
        let running = {
            let executor = executor.clone();
            tokio::spawn(async move {
                call(
                    &executor,
                    "bash",
                    json!({"command":command, "timeout":35, "yield_time_ms":30000}),
                )
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !executor.foreground_wait_control.is_active() || !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("foreground shell must actually start");
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while registry.running_count() != 0 {
                registry.poll_completions();
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("abandoned shell must settle");
    }
    registry.poll_completions();
    registry.prune_retained_terminal_tasks();

    let tasks = registry.all_tasks().collect::<Vec<_>>();
    // The test configuration retains at most three successes and three
    // failures; repeated dropped waiters must not accumulate invisibly.
    assert!(!tasks.is_empty() && tasks.len() <= 6, "{}", tasks.len());
    assert!(tasks.iter().all(|task| task.status().is_terminal()));
    let next = call(&executor, "bash", json!({"command":"printf usable"})).await;
    assert!(!next.is_error && next.output.contains("usable"), "{next:?}");
}

#[tokio::test]
async fn observing_unknown_shell_result_delivers_the_execution_fact() {
    let (root, executor) = fixture();
    let stdout = root.path().join("stdout");
    let stderr = root.path().join("stderr");
    std::fs::write(&stdout, "partial output\n").unwrap();
    std::fs::write(&stderr, "").unwrap();
    let mut task = executor
        .background_tasks
        .start_shell(
            "unknown result",
            (stdout, stderr, None),
            true,
            false,
            None,
            |_, _| async {
                let mut outcome =
                    astra_tools::execution_outcome::ToolExecutionOutcome::error_with_evidence(
                        "outcome unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    );
                outcome
                    .tool_result_fields
                    .as_mut()
                    .unwrap()
                    .insert("cleanup_authoritative".into(), json!(true));
                outcome
            },
        )
        .unwrap();
    task.wait().await.unwrap();
    let result = call(&executor, "task_output", json!({"task_id":task.id})).await;
    let snapshot: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(snapshot["execution"]["execution_fact"], "unknown");
    assert!(
        snapshot["execution_notice"]
            .as_str()
            .unwrap()
            .contains("may have side effects")
    );
    let mut registry = executor.background_tasks.clone();
    executor.acknowledge_delivered_task_result("task_input", result.tool_result_fields.as_ref());
    let before_delivery = registry.kill_all_and_wait(Duration::from_secs(1)).await;
    assert_eq!(before_delivery.unknown, [task.id.clone()]);
    executor.acknowledge_delivered_task_result("task_output", result.tool_result_fields.as_ref());
    let report = registry.kill_all_and_wait(Duration::from_secs(1)).await;
    assert!(
        report.unknown.is_empty() && report.unsettled.is_empty(),
        "{report:?}"
    );
}

#[tokio::test]
async fn missing_capture_preserves_unknown_execution_context() {
    let (root, executor) = fixture();
    let mut task = executor
        .background_tasks
        .start_shell(
            "missing capture",
            (
                root.path().join("missing-out"),
                root.path().join("missing-err"),
                None,
            ),
            true,
            false,
            None,
            |_, _| async {
                let mut outcome =
                    astra_tools::execution_outcome::ToolExecutionOutcome::error_with_evidence(
                        "outcome unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    );
                outcome
                    .tool_result_fields
                    .as_mut()
                    .unwrap()
                    .insert("cleanup_authoritative".into(), json!(true));
                outcome
            },
        )
        .unwrap();
    task.wait().await.unwrap();

    let mut last_fields = None;
    for args in [
        json!({"task_id":format!("  {}  ", task.id)}),
        json!({"task_id":task.id, "pattern":"anything"}),
    ] {
        let result = call(&executor, "task_output", args).await;
        let snapshot: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            matches!(
                snapshot["status"].as_str(),
                Some("output_missing" | "output_unavailable")
            ),
            "{snapshot}"
        );
        assert_eq!(
            snapshot["task"]["execution"]["execution_fact"], "unknown",
            "{snapshot}"
        );
        assert!(
            snapshot["execution_notice"]
                .as_str()
                .unwrap()
                .contains("side effects")
        );
        assert_eq!(
            result.tool_result_fields.as_ref().unwrap()["task"]["task_id"],
            task.id
        );
        let mut registry = executor.background_tasks.clone();
        assert_eq!(
            registry
                .kill_all_and_wait(Duration::from_secs(1))
                .await
                .unknown,
            [task.id.clone()]
        );
        last_fields = result.tool_result_fields;
    }
    executor.acknowledge_delivered_task_result("task_output", last_fields.as_ref());
    let mut registry = executor.background_tasks.clone();
    assert!(
        registry
            .kill_all_and_wait(Duration::from_secs(1))
            .await
            .unknown
            .is_empty()
    );
}

#[tokio::test]
async fn live_and_terminal_task_output_redact_json_escaped_arguments() {
    let (_root, executor) = fixture();
    let command =
        r#"printf '%s\n' '{"cmd":"--token \"quoted-test-secret-987654321\""}'; read -r response"#;
    let id = launch(&executor, command, true, 10.0).await;
    stdin_ready(&executor, &id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = call(&executor, "task_output", json!({"task_id":id})).await;
            let output = output_chunk(&result);
            assert!(!output.contains("quoted-test-secret-987654321"), "{output}");
            if output.contains("TOKEN_ARGUMENT") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("live output must arrive");
    let search = call(
        &executor,
        "task_output",
        json!({"task_id":id,"pattern":"cmd"}),
    )
    .await;
    assert!(
        !search.output.contains("quoted-test-secret-987654321"),
        "{}",
        search.output
    );
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"done\n"}),
    )
    .await;
    assert!(!input.is_error, "{input:?}");
    terminal(&executor, &id).await;
    for args in [json!({"task_id":id}), json!({"task_id":id,"pattern":"cmd"})] {
        let result = call(&executor, "task_output", args).await;
        assert!(
            !result.output.contains("quoted-test-secret-987654321"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("TOKEN_ARGUMENT"),
            "{}",
            result.output
        );
    }
}

#[tokio::test]
async fn chain_preserves_each_unknown_shell_until_parent_delivery() {
    let (root, executor) = fixture();
    let mut ids = Vec::new();
    for index in 0..2 {
        let stdout = root.path().join(format!("chain-out-{index}"));
        let stderr = root.path().join(format!("chain-err-{index}"));
        std::fs::write(&stdout, "partial\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut task = executor
            .background_tasks
            .start_shell(
                "unknown",
                (stdout, stderr, None),
                true,
                false,
                None,
                |_, _| async {
                    let mut outcome =
                        astra_tools::execution_outcome::ToolExecutionOutcome::error_with_evidence(
                            "unknown".into(),
                            astra_core::ToolFailureEvidence::from_error_kind(
                                astra_core::ErrorKind::ToolOutcomeUnknown,
                            ),
                        );
                    outcome
                        .tool_result_fields
                        .as_mut()
                        .unwrap()
                        .insert("cleanup_authoritative".into(), json!(true));
                    outcome
                },
            )
            .unwrap();
        task.wait().await.unwrap();
        ids.push(task.id);
    }
    let chain = astra_turn_core::tool_registry_chain::ToolChain::new("observe", "Observe tasks")
        .step("task_output", json!({"task_id":ids[0]}))
        .step("task_output", json!({"task_id":ids[1]}));
    let result = executor
        .execute_chain_outcome(&chain, Value::Null, None)
        .await;
    assert_eq!(
        result.tool_result_fields.as_ref().unwrap()["nested_unknown_shell_tasks"],
        json!(ids)
    );
    let mut registry = executor.background_tasks.clone();
    assert_eq!(
        registry
            .kill_all_and_wait(Duration::from_secs(1))
            .await
            .unknown
            .len(),
        2
    );
    executor.acknowledge_delivered_task_result("agent", result.tool_result_fields.as_ref());
    assert!(
        registry
            .kill_all_and_wait(Duration::from_secs(1))
            .await
            .unknown
            .is_empty()
    );
}

#[tokio::test]
async fn task_controls_redact_quoted_secrets_before_serialization() {
    let (_root, executor) = fixture();
    let secret = "quoted-secret-value-987654321";
    let command = format!("printf done # --token \"{secret}\"");
    let id = launch(&executor, &command, false, 5.0).await;
    terminal(&executor, &id).await;
    for (name, args) in [
        ("task_output", json!({"task_id": id})),
        (
            "task_output",
            json!({"task_id": id, "pattern": format!("--token \"{secret}\"")}),
        ),
        ("task_list", json!({})),
    ] {
        let result = call(&executor, name, args).await;
        assert!(!result.is_error, "{result:?}");
        assert!(
            !result.output.contains(secret),
            "{name} leaked a credential"
        );
        assert!(
            !serde_json::to_string(&result.tool_result_fields)
                .unwrap()
                .contains(secret),
            "{name} metadata leaked a credential"
        );
    }
}

#[tokio::test]
async fn interactive_unfinished_sensitive_value_has_actionable_feedback() {
    let (_root, executor) = fixture();
    let id = launch(
        &executor,
        "printf 'Open http://127.0.0.1:%s' \"$$\"; IFS= read -r answer; test \"$answer\" = \"$$\" && printf '\\nMATCHED\\n'",
        true,
        10.0,
    )
    .await;
    stdin_ready(&executor, &id).await;
    let path = executor.background_tasks.get(&id).unwrap().stdout_path;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !std::fs::read_to_string(&path).is_ok_and(|text| {
            text.strip_prefix("Open http://127.0.0.1:")
                .is_some_and(|port| port.parse::<u32>().is_ok())
        }) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let live = call(&executor, "task_output", json!({"task_id":id})).await;
    let live: Value = serde_json::from_str(&live.output).unwrap();
    assert!(
        live["output"]
            .as_str()
            .unwrap()
            .contains("URL_AUTHORITY_PENDING")
    );
    assert!(
        live["withheld_output"]
            .as_str()
            .unwrap()
            .contains("local task panel")
    );
    assert!(!live["output"].as_str().unwrap().contains("127.0.0.1"));
    let local = executor
        .background_tasks
        .local_input_preview(&id)
        .await
        .unwrap();
    let answer = local.0.rsplit_once(':').unwrap().1.parse::<u32>().unwrap();
    let sent = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":format!("{answer}\n")}),
    )
    .await;
    assert!(!sent.is_error, "{sent:?}");
    terminal(&executor, &id).await;
    let done = call(&executor, "task_output", json!({"task_id":id})).await;
    let done: Value = serde_json::from_str(&done.output).unwrap();
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .contains(&format!("http://127.0.0.1:{answer}"))
    );
    assert!(done["output"].as_str().unwrap().contains("MATCHED"));
    assert_eq!(done["status"], "completed");
}

#[tokio::test]
async fn local_output_is_available_when_the_command_has_no_stdin() {
    let (_root, executor) = fixture();
    let id = launch(
        &executor,
        "printf 'http://127.0.0.1:%s' \"$$\"; sleep 2",
        false,
        5.0,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let local = executor
                .background_tasks
                .local_input_preview(&id)
                .await
                .unwrap();
            if local.0.starts_with("http://127.0.0.1:") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !executor
            .background_tasks
            .get(&id)
            .unwrap()
            .stdin_available()
    );
    let live = call(&executor, "task_output", json!({"task_id":id})).await;
    let live: Value = serde_json::from_str(&live.output).unwrap();
    let output = live["output"].as_str().unwrap();
    assert!(output.contains("URL_AUTHORITY_PENDING"), "{live:?}");
    assert!(!output.contains("127.0.0.1"));
    terminal(&executor, &id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn task_output_reads_the_original_capture_after_path_replacement() {
    use std::os::unix::fs::symlink;

    let (root, executor) = fixture();
    let id = launch(
        &executor,
        "printf 'owned output\\n'; IFS= read -r answer",
        true,
        10.0,
    )
    .await;
    stdin_ready(&executor, &id).await;
    let capture = executor.background_tasks.get(&id).unwrap().stdout_path;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !std::fs::read_to_string(&capture).is_ok_and(|s| s.contains("owned output")) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The file flush can complete just before the live projection publishes
    // its revision, so wait on the public observation rather than the path.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let first = call(&executor, "task_output", json!({"task_id": id})).await;
            if output_chunk(&first).contains("owned output") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let original = capture.with_extension("saved");
    let substituted = root.path().join("unrelated-output");
    std::fs::write(&substituted, "unrelated replacement").unwrap();
    std::fs::rename(&capture, &original).unwrap();
    symlink(&substituted, &capture).unwrap();
    for args in [
        json!({"task_id": id}),
        json!({"task_id": id, "pattern": "replacement"}),
    ] {
        let observed = call(&executor, "task_output", args).await;
        assert!(
            !observed.output.contains("unrelated replacement"),
            "{observed:?}"
        );
        assert!(
            !observed.output.contains(root.path().to_str().unwrap()),
            "{observed:?}"
        );
        assert!(
            !serde_json::to_string(&observed.tool_result_fields)
                .unwrap()
                .contains(root.path().to_str().unwrap())
        );
        assert!(
            observed.is_error
                || observed.output.contains("unavailable")
                || observed.output.contains("Live search covers")
                || output_chunk(&observed).contains("owned output"),
            "{observed:?}"
        );
    }
    let sent = call(
        &executor,
        "task_input",
        json!({"task_id": id, "input": "done\n"}),
    )
    .await;
    assert!(!sent.is_error, "{sent:?}");
    terminal(&executor, &id).await;
    let settled = call(&executor, "task_output", json!({"task_id":id,"offset":0})).await;
    assert!(
        !settled.is_error && output_chunk(&settled).contains("owned output"),
        "{settled:?}"
    );
    let searched = call(
        &executor,
        "task_output",
        json!({"task_id":id,"pattern":"owned output"}),
    )
    .await;
    assert!(
        !searched.is_error && searched.output.contains("owned output"),
        "{searched:?}"
    );
    assert!(!settled.output.contains("unrelated replacement"));
    std::fs::remove_file(&capture).unwrap();
    std::fs::rename(&original, &capture).unwrap();
}

#[tokio::test]
async fn independent_sessions_can_use_the_same_workspace_while_shell_runs() {
    let (workspace, executor) = fixture();
    let id = launch(
        &executor,
        "touch started; while [ ! -f release ]; do sleep 0.1; done",
        false,
        30.0,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !workspace.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let mut commands = tokio::task::JoinSet::new();
    for index in 0..8 {
        let root = workspace.path().to_path_buf();
        commands.spawn(async move {
            let other = ToolExecutor::new(&root);
            let command = format!("touch ready-{index}; while [ ! -f release-peers ]; do sleep 0.1; done; printf independent");
            call(
                &other,
                "bash",
                json!({"command":command, "timeout":25}),
            )
            .await
        });
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while (0..8).any(|index| !workspace.path().join(format!("ready-{index}")).exists()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all independent shells should run concurrently");
    let other = ToolExecutor::new(workspace.path());
    let write = tokio::time::timeout(
        Duration::from_secs(3),
        call(
            &other,
            "write_file",
            json!({"path":"unrelated", "content":"written"}),
        ),
    )
    .await
    .expect("a file edit should not wait for an unrelated shell");
    assert!(!write.is_error, "{write:?}");
    std::fs::write(workspace.path().join("release-peers"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        while let Some(command) = commands.join_next().await {
            let command = command.unwrap();
            assert!(
                !command.is_error && command.output.contains("independent"),
                "{command:?}"
            );
        }
    })
    .await
    .expect("independent shells should finish after release");
    assert!(
        !executor
            .background_tasks
            .get(&id)
            .unwrap()
            .status()
            .is_terminal()
    );
    std::fs::write(workspace.path().join("release"), "").unwrap();
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn interactive_shell_output_is_visible_and_live_offsets_are_rejected() {
    let (_workspace, executor) = fixture();
    let id = launch(
        &executor,
        "printf 'Confirm? '; printf 'warning\\n' >&2; read answer; printf 'answer=%s\\n' \"$answer\"",
        true,
        10.0,
    )
    .await;
    stdin_ready(&executor, &id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let output = call(&executor, "task_output", json!({"task_id":id})).await;
            assert!(!output.is_error, "{output:?}");
            if output_chunk(&output).contains("Confirm?")
                && output_chunk(&output).contains("warning")
            {
                assert!(!output.output.contains("next_offset"), "{output:?}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("an unfinished-line prompt should be visible before input");
    let tiny = call(
        &executor,
        "task_output",
        json!({"task_id":id,"max_bytes":1}),
    )
    .await;
    assert!(
        tiny.is_error && tiny.output.contains("too small to show both streams"),
        "{tiny:?}"
    );
    let invalid = call(&executor, "task_output", json!({"task_id":id,"offset":0})).await;
    assert!(invalid.is_error, "{invalid:?}");
    assert!(
        invalid.output.contains("after the task ends"),
        "{invalid:?}"
    );
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"yes\n"}),
    )
    .await;
    assert!(!input.is_error, "{input:?}");
    assert!(!terminal(&executor, &id).await.is_error);
    let output = call(&executor, "task_output", json!({"task_id":id,"offset":0})).await;
    assert!(!output.is_error, "{output:?}");
    assert!(
        output.output.contains("answer=yes") && output.output.contains("warning"),
        "{output:?}"
    );
}

#[tokio::test]
async fn interactive_wait_observes_unterminated_output_and_preserves_input() {
    let (workspace, executor) = fixture();
    let id = launch(
        &executor,
        "IFS= read -r gate; printf 'Password:'; IFS= read -r answer",
        true,
        10.0,
    )
    .await;
    stdin_ready(&executor, &id).await;
    let observer = Arc::new(
        ToolExecutor::new(workspace.path())
            .with_background_tasks(executor.background_tasks.clone()),
    );
    let waiter = {
        let (executor, id) = (executor.clone(), id.clone());
        tokio::spawn(async move {
            call(
                &executor,
                "task_output",
                json!({"task_id":id,"block":true,"wake_on_output":true,"timeout_ms":5000}),
            )
            .await
        })
    };
    let second_waiter = {
        let (observer, id) = (observer.clone(), id.clone());
        tokio::spawn(async move {
            call(
                &observer,
                "task_output",
                json!({"task_id":id,"block":true,"wake_on_output":true,"timeout_ms":5000}),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !executor.foreground_wait_control.is_active()
            || !observer.foreground_wait_control.is_active()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"go\n"}),
    )
    .await;
    assert!(!input.is_error, "{input:?}");
    let observed = tokio::time::timeout(Duration::from_secs(3), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !observed.is_error && output_chunk(&observed).contains("Password:"),
        "{observed:?}"
    );
    assert_eq!(
        observed.tool_result_fields.as_ref().unwrap()["wait_result"],
        "output"
    );
    let second = tokio::time::timeout(Duration::from_secs(3), second_waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !second.is_error && output_chunk(&second).contains("Password:"),
        "{second:?}"
    );
    let again = call(
        &executor,
        "task_output",
        json!({"task_id":id,"block":true,"wake_on_output":true,"timeout_ms":1000}),
    )
    .await;
    assert!(
        !again.is_error && output_chunk(&again).contains("Password:"),
        "{again:?}"
    );
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"ok\n"}),
    )
    .await;
    assert!(!input.is_error, "{input:?}");
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn interactive_observer_handles_fragmented_prompt_output() {
    let (_workspace, executor) = fixture();
    let id = launch(
        &executor,
        "python3 -u -c 'import sys,time; sys.stdout.write(\"FIRST> \" ); sys.stdout.flush(); sys.stdin.readline(); [(sys.stdout.write(c),sys.stdout.flush(),time.sleep(0.01)) for c in \"0123456789abcdef\\n\"]; sys.stdin.readline()'",
        true,
        10.0,
    )
    .await;
    stdin_ready(&executor, &id).await;
    let first = call(
        &executor,
        "task_output",
        json!({
            "task_id":id, "block":true, "wake_on_output":true, "timeout_ms":4000
        }),
    )
    .await;
    let revision = first.tool_result_fields.as_ref().unwrap()["background_task_observation"]
        ["output_revision"].as_u64().unwrap();
    let waiter = {
        let (executor, id) = (executor.clone(), id.clone());
        tokio::spawn(
            async move { observe_challenge(&executor, &id, "FIRST> ", Some(revision)).await },
        )
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !executor.foreground_wait_control.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let start = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"go\n"}),
    )
    .await;
    assert!(!start.is_error, "{start:?}");
    let (_, code) = waiter.await.unwrap();
    assert_eq!(code, "0123456789abcdef");
    let done = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"done\n"}),
    )
    .await;
    assert!(!done.is_error, "{done:?}");
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn interactive_wait_can_follow_new_activity_without_replaying_old_prompt() {
    let (workspace, executor) = fixture();
    std::fs::write(
        workspace.path().join("challenge.py"),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/harness/interactive-shell/challenge.py"
        )),
    )
    .unwrap();
    let id = launch(&executor, "python3 -u challenge.py", true, 10.0).await;
    stdin_ready(&executor, &id).await;
    let (first, first_code) = observe_challenge(&executor, &id, "FIRST> ", None).await;
    let mut revision = first.tool_result_fields.as_ref().unwrap()["background_task_observation"]
        ["output_revision"].as_u64().unwrap();
    assert!(
        serde_json::from_str::<Value>(&first.output).unwrap()["output_revision"] == revision,
        "{first:?}"
    );

    // The snapshot can include bytes that raced with its sampled revision.
    // Wait until that activity has settled before asserting an idle wait.
    let no_change = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = call(
                &executor,
                "task_output",
                json!({
                    "task_id":id, "block":true, "wake_on_output":true,
                    "after_output_revision":revision, "timeout_ms":100
                }),
            )
            .await;
            assert!(!result.is_error, "{result:?}");
            revision = result.tool_result_fields.as_ref().unwrap()["background_task_observation"]
                ["output_revision"].as_u64().unwrap();
            if result.tool_result_fields.as_ref().unwrap()["wait_result"] == "timeout" {
                break result;
            }
        }
    })
    .await
    .expect("first prompt activity settles");
    assert!(!no_change.is_error, "{no_change:?}");
    assert_eq!(
        no_change.tool_result_fields.as_ref().unwrap()["wait_result"],
        "timeout"
    );
    assert!(
        !executor
            .background_tasks
            .get(&id)
            .unwrap()
            .status()
            .is_terminal()
    );

    let future = call(
        &executor,
        "task_output",
        json!({
            "task_id":id, "block":true, "wake_on_output":true,
            "after_output_revision":u64::MAX, "timeout_ms":1000
        }),
    )
    .await;
    assert!(
        future.is_error && future.output.contains("newer"),
        "{future:?}"
    );

    let observer = Arc::new(
        ToolExecutor::new(workspace.path())
            .with_background_tasks(executor.background_tasks.clone()),
    );
    let mut waiters = Vec::new();
    for owner in [executor.clone(), observer.clone()] {
        let id = id.clone();
        waiters.push(tokio::spawn(async move {
            observe_challenge(&owner, &id, "SECOND> ", Some(revision)).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while !executor.foreground_wait_control.is_active()
            || !observer.foreground_wait_control.is_active()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both observers started waiting");
    assert!(waiters.iter().all(|waiter| !waiter.is_finished()));
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id, "input":format!("{first_code}\n")}),
    )
    .await;
    assert!(!input.is_error, "{input:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&input.output).unwrap()["input_delivery"],
        "written"
    );
    let mut second_code = None;
    for waiter in waiters {
        let (next, code) = waiter.await.unwrap();
        assert!(
            !next.is_error && next.output.contains("SECOND>"),
            "{next:?}"
        );
        if let Some(previous) = &second_code {
            assert_eq!(previous, &code);
        } else {
            second_code = Some(code);
        }
        assert_eq!(
            next.tool_result_fields.as_ref().unwrap()["wait_result"],
            "output"
        );
        assert!(next.tool_result_fields.as_ref().unwrap()["background_task_observation"]
            ["output_revision"].as_u64().unwrap() > revision, "{next:?}");
    }
    // Activity which arrives before a subsequent wait must also be observed.
    let already_changed = call(
        &executor,
        "task_output",
        json!({
            "task_id":id, "block":true, "wake_on_output":true,
            "after_output_revision":revision, "timeout_ms":1000
        }),
    )
    .await;
    assert!(
        !already_changed.is_error && already_changed.output.contains("SECOND>"),
        "{already_changed:?}"
    );
    for args in [
        json!({"task_id":id, "after_output_revision":revision}),
        json!({"task_id":id, "block":true, "wake_on_output":true, "after_output_revision":revision, "offset":0}),
        json!({"task_id":id, "block":true, "wake_on_output":true, "after_output_revision":revision, "pattern":"SECOND"}),
    ] {
        let invalid = call(&executor, "task_output", args).await;
        assert!(invalid.is_error, "{invalid:?}");
    }
    let closed = call(
        &executor,
        "task_input",
        json!({"task_id":id, "input":format!("{}\n", second_code.unwrap())}),
    )
    .await;
    assert!(!closed.is_error, "{closed:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&closed.output).unwrap()["input_delivery"],
        "written"
    );
    assert!(!terminal(&executor, &id).await.is_error);
    let final_output = call(&executor, "task_output", json!({"task_id":id})).await;
    let final_value: Value = serde_json::from_str(&final_output.output).unwrap();
    assert_eq!(final_value["terminal"], true);
    assert_eq!(final_value["status"], "completed");
    assert_eq!(final_value["offset"], 0);
    assert_eq!(final_value["execution"]["execution_fact"], "executed");
    assert_eq!(final_value["execution"]["exit_code"], 0);
    assert!(output_chunk(&final_output).contains("INTERACTIVE_OK"));
}

#[tokio::test]
async fn live_shell_output_keeps_listening_url_visible() {
    let (_workspace, executor) = fixture();
    let id = launch(
        &executor,
        "printf 'Listening on http://127.0.0.1:3000\\n'; sleep 5",
        false,
        10.0,
    )
    .await;
    let observed = call(
        &executor,
        "task_output",
        json!({
            "task_id":id, "block":true, "wake_on_output":true, "timeout_ms":4000
        }),
    )
    .await;
    let _ = call(&executor, "task_stop", json!({"task_id":id})).await;
    let _ = terminal(&executor, &id).await;
    assert!(
        observed.output.contains("http://127.0.0.1:3000"),
        "{observed:?}"
    );
}

#[tokio::test]
async fn live_output_keeps_both_streams_visible_after_a_noisy_stream() {
    let (_workspace, executor) = fixture();
    for command in [
        "yes warning | head -n 12000 >&2; printf 'READY:'; IFS= read -r answer",
        "yes warning | head -n 12000; printf 'READY:' >&2; IFS= read -r answer",
    ] {
        let id = launch(&executor, command, true, 10.0).await;
        stdin_ready(&executor, &id).await;
        let handle = executor.background_tasks.get(&id).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let out = std::fs::read(&handle.stdout_path).unwrap_or_default();
                let err = std::fs::read(&handle.stderr_path).unwrap_or_default();
                if out.windows(6).any(|w| w == b"READY:") || err.windows(6).any(|w| w == b"READY:")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let output = call(
            &executor,
            "task_output",
            json!({"task_id":id,"max_bytes":65536}),
        )
        .await;
        assert!(
            !output.is_error
                && output_chunk(&output).contains("READY:")
                && output_chunk(&output).contains("warning"),
            "{output:?}"
        );
        let input = call(
            &executor,
            "task_input",
            json!({"task_id":id,"input":"ok\n"}),
        )
        .await;
        assert!(!input.is_error, "{input:?}");
        assert!(!terminal(&executor, &id).await.is_error);
    }
}

#[tokio::test]
async fn a_removed_capture_path_does_not_hide_either_live_stream() {
    let (_workspace, executor) = fixture();
    for (command, broken_stdout) in [
        ("printf 'READY:'; IFS= read -r answer", false),
        ("printf 'READY:' >&2; IFS= read -r answer", true),
    ] {
        let id = launch(&executor, command, true, 10.0).await;
        stdin_ready(&executor, &id).await;
        let handle = executor.background_tasks.get(&id).unwrap();
        let (working, broken) = if broken_stdout {
            (handle.stderr_path, handle.stdout_path)
        } else {
            (handle.stdout_path, handle.stderr_path)
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while !std::fs::read_to_string(&working).is_ok_and(|s| s.contains("READY:")) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        std::fs::remove_file(broken).unwrap();
        let output = call(&executor, "task_output", json!({"task_id":id})).await;
        assert!(
            !output.is_error && output.output.contains("READY:"),
            "{output:?}"
        );
        let search = call(
            &executor,
            "task_output",
            json!({"task_id":id,"pattern":"READY:"}),
        )
        .await;
        assert!(
            !search.is_error
                && search.output.contains("READY:")
                && search.output.contains("Live search covers only"),
            "{search:?}"
        );
        let input = call(
            &executor,
            "task_input",
            json!({"task_id":id,"input":"ok\n"}),
        )
        .await;
        assert!(!input.is_error, "{input:?}");
        let _ = terminal(&executor, &id).await;
    }
}

#[tokio::test]
async fn incomplete_terminal_capture_remains_observable_and_searchable() {
    let (_workspace, executor) = fixture();
    let id = launch(&executor, "printf 'failure-detail'; sleep 4", false, 1.0).await;
    let done = terminal(&executor, &id).await;
    assert!(done.is_error);
    assert_eq!(
        done.tool_result_fields.as_ref().unwrap()["bash_output_capture"]["complete"],
        false
    );
    let output = call(&executor, "task_output", json!({"task_id":id,"offset":0})).await;
    assert!(
        !output.is_error && output_chunk(&output).contains("failure-detail"),
        "{output:?}"
    );
    let search = call(
        &executor,
        "task_output",
        json!({"task_id":id,"pattern":"failure-detail"}),
    )
    .await;
    assert!(
        !search.is_error
            && search
                .output
                .split_once("Matched output:\n")
                .is_some_and(|(_, chunk)| chunk.contains("failure-detail")),
        "{search:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn signalled_task_output_does_not_expose_incomplete_credential() {
    let (workspace, executor) = fixture();
    std::fs::write(
        workspace.path().join("signal_helper.py"),
        "import os, signal\nos.write(1, b'SIGNAL_FIXTURE_STARTED\\npassword=abc')\nos.kill(os.getpid(), signal.SIGKILL)\n",
    )
    .unwrap();
    let id = launch(&executor, "python3 signal_helper.py", false, 5.0).await;
    let done = terminal(&executor, &id).await;
    assert!(done.is_error, "{done:?}");
    assert_eq!(
        done.tool_result_fields.as_ref().unwrap()["execution_started"],
        true
    );
    assert_eq!(done.tool_result_fields.as_ref().unwrap()["exit_code"], 137);
    assert_eq!(
        done.tool_result_fields.as_ref().unwrap()["bash_output_capture"]["complete"],
        false
    );
    let output = call(&executor, "task_output", json!({"task_id":id,"offset":0})).await;
    assert!(!output.is_error, "{output:?}");
    assert!(
        output.output.contains("SIGNAL_FIXTURE_STARTED"),
        "{output:?}"
    );
    assert!(!output.output.contains("password=abc"), "{output:?}");
}

#[tokio::test]
async fn task_output_timeout_does_not_stop_an_interactive_process() {
    let (_workspace, executor) = fixture();
    let id = launch(&executor, "cat", true, 10.0).await;
    stdin_ready(&executor, &id).await;
    for wake_on_output in [false, true] {
        let result = call(
            &executor,
            "task_output",
            json!({
                "task_id":id, "block":true, "wake_on_output":wake_on_output, "timeout_ms":100
            }),
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            result.tool_result_fields.as_ref().unwrap()["wait_result"],
            "timeout"
        );
        assert!(
            !executor
                .background_tasks
                .get(&id)
                .unwrap()
                .status()
                .is_terminal()
        );
    }
    let closed = call(
        &executor,
        "task_input",
        json!({"task_id":id,"close_stdin":true}),
    )
    .await;
    assert!(!closed.is_error, "{closed:?}");
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn unsupported_source_capture_does_not_start_local_command() {
    let (workspace, executor) = fixture();
    let output = call(
        &executor,
        "bash",
        json!({"command":"touch marker", "source_artifacts":["input.txt"]}),
    )
    .await;
    assert!(output.is_error, "{output:?}");
    assert!(!workspace.path().join("marker").exists());
}

#[tokio::test]
async fn large_foreground_output_uses_existing_task_controls_without_workspace_files() {
    let (workspace, executor) = fixture();
    let result = call(
        &executor,
        "bash",
        json!({"command":"seq 1 20000; printf diagnostic >&2; exit 7"}),
    )
    .await;
    assert!(result.is_error, "{result:?}");
    let fields = result.tool_result_fields.as_ref().unwrap();
    assert_eq!(fields["exit_code"], 7);
    let id = fields["task_id"].as_str().unwrap();
    assert!(result.output.contains("task_output"));
    assert!(!workspace.path().join("astra-bash-outputs").exists());
    let mut registry = executor.background_tasks.clone();
    assert_eq!(registry.all_tasks().count(), 0);
    assert_eq!(registry.failed_count(), 0);
    assert!(registry.poll_completions().is_empty());
    for _ in 0..8 {
        let next = call(&executor, "bash", json!({"command":"printf ok"})).await;
        assert!(!next.is_error, "{next:?}");
    }
    let first = call(&executor, "task_output", json!({"task_id":id,"offset":0})).await;
    assert!(
        !first.is_error && output_chunk(&first).contains("1\n2\n"),
        "{first:?}"
    );
    let last = call(
        &executor,
        "task_output",
        json!({"task_id":id,"pattern":"20000"}),
    )
    .await;
    assert!(!last.is_error && last.output.contains("20000"), "{last:?}");
    let private = executor.background_tasks.get(id).unwrap().stdout_path;
    assert!(private.exists());
    assert_eq!(registry.all_tasks().count(), 0);
    assert!(registry.poll_completions().is_empty());
    drop(registry);
    drop(executor);
    assert!(
        !private.exists(),
        "session close must reclaim its private capture"
    );
}

#[tokio::test]
async fn capture_limit_does_not_stop_the_command_or_claim_a_complete_search() {
    let (workspace, executor) = fixture();
    let id = launch(
        &executor,
        "python3 -u -c 'import sys,pathlib; sys.stdout.write(\"x\"*53000000+\"\\nREADY_AFTER_LIMIT\\n\"); sys.stdout.flush(); pathlib.Path(\"command-ready\").touch(); sys.stdin.readline(); print(\"SECOND_AFTER_LIMIT\",flush=True); sys.stdin.readline(); pathlib.Path(\"command-finished\").touch()'",
        true,
        60.0,
    ).await;
    stdin_ready(&executor, &id).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while !workspace.path().join("command-ready").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("process reached its prompt after the capture cap");
    let current = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = call(&executor, "task_output", json!({"task_id":id})).await;
            let current: Value = serde_json::from_str(&current.output).unwrap();
            if current["output"]
                .as_str()
                .is_some_and(|s| s.contains("READY_AFTER_LIMIT"))
            {
                break current;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("post-cap output must reach the live view");
    assert_eq!(current["output_complete"], false);
    assert!(
        current["output"]
            .as_str()
            .unwrap()
            .contains("READY_AFTER_LIMIT"),
        "{current:?}"
    );
    assert!(
        executor
            .background_tasks
            .local_input_preview(&id)
            .await
            .unwrap()
            .0
            .contains("READY_AFTER_LIMIT")
    );
    let revision = current["output_revision"].as_u64().unwrap();
    let unchanged = call(
        &executor,
        "task_output",
        json!({
            "task_id":id,"block":true,"wake_on_output":true,
            "after_output_revision":revision,"timeout_ms":100
        }),
    )
    .await;
    assert_eq!(
        unchanged.tool_result_fields.as_ref().unwrap()["wait_result"],
        "timeout"
    );

    let waiter = {
        let (executor, id) = (executor.clone(), id.clone());
        tokio::spawn(async move {
            call(
                &executor,
                "task_output",
                json!({
                    "task_id":id,"block":true,"wake_on_output":true,
                    "after_output_revision":revision,"timeout_ms":5000
                }),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !executor.foreground_wait_control.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"go\n"}),
    )
    .await;
    assert!(!input.is_error, "{input:?}");
    let updated = waiter.await.unwrap();
    assert!(
        output_chunk(&updated).contains("SECOND_AFTER_LIMIT"),
        "{updated:?}"
    );
    assert_eq!(
        updated.tool_result_fields.as_ref().unwrap()["wait_result"],
        "output"
    );
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"done\n"}),
    )
    .await;
    assert!(!input.is_error, "{input:?}");
    let result = terminal(&executor, &id).await;
    assert!(!result.is_error, "{result:?}");
    assert!(workspace.path().join("command-finished").exists());
    let fields = result.tool_result_fields.as_ref().unwrap();
    assert_eq!(fields["exit_code"], 0);
    assert_eq!(fields["bash_output_capture"]["complete"], false);
    let latest = call(&executor, "task_output", json!({"task_id":id})).await;
    let latest: Value = serde_json::from_str(&latest.output).unwrap();
    assert_eq!(latest["terminal"], true);
    assert!(
        latest["output"]
            .as_str()
            .unwrap()
            .contains("SECOND_AFTER_LIMIT"),
        "{latest:?}"
    );
    assert!(
        latest.get("next_offset").is_none(),
        "live tail is not a historical cursor"
    );
    let page = call(&executor, "task_output", json!({"task_id":id,"offset":0})).await;
    let page: Value = serde_json::from_str(&page.output).unwrap();
    assert_eq!(page["output_complete"], false);
    let search = call(
        &executor,
        "task_output",
        json!({"task_id":id,"pattern":"not-present"}),
    )
    .await;
    assert!(
        search.output.contains("search covers captured output only"),
        "{search:?}"
    );
}

#[tokio::test]
async fn foreground_waits_until_completion_and_explicit_controls_remain_available() {
    let (_workspace, executor) = fixture();
    let result = call(
        &executor,
        "bash",
        json!({"command":"printf ok", "stdin":false}),
    )
    .await;
    assert!(!result.is_error && result.output == "ok", "{result:?}");
    let completed = call(
        &executor,
        "bash",
        json!({"command":"sleep 10.2; printf finished", "timeout":20}),
    )
    .await;
    assert!(
        !completed.is_error && completed.output.contains("finished"),
        "{completed:?}"
    );
    assert_eq!(
        completed.tool_result_fields.as_ref().unwrap()["exit_code"],
        0
    );

    let interactive = call(
        &executor,
        "bash",
        json!({"command":"read answer; printf 'received:%s' \"$answer\"", "stdin":true}),
    )
    .await;
    assert!(!interactive.is_error, "{interactive:?}");
    let id = interactive.tool_result_fields.as_ref().unwrap()["task_id"]
        .as_str()
        .unwrap();
    stdin_ready(&executor, id).await;
    let sent = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"hello\n"}),
    )
    .await;
    assert!(!sent.is_error, "{sent:?}");
    let outcome = terminal(&executor, id).await;
    assert!(
        !outcome.is_error && outcome.output.contains("received:hello"),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn unavailable_capture_falls_back_for_foreground_but_rejects_controls() {
    let (workspace, _) = fixture();
    let output_root = workspace.root.path().join("occupied");
    std::fs::write(&output_root, b"not a directory").unwrap();
    let registry = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(output_root);
    let executor = ToolExecutor::new(workspace.path()).with_background_tasks(registry);
    let failed = call(
        &executor,
        "bash",
        json!({"command":"printf x >> executed; printf stdout; printf stderr >&2; exit 7"}),
    )
    .await;
    assert!(failed.is_error, "{failed:?}");
    assert_eq!(failed.tool_result_fields.as_ref().unwrap()["exit_code"], 7);
    assert!(failed.output.contains("stdout") && failed.output.contains("stderr"));
    assert_eq!(
        std::fs::read(workspace.path().join("executed")).unwrap(),
        b"x"
    );
    assert!(!failed.output.contains("task_output"), "{failed:?}");

    let preview_limit = executor
        .scaled_output_limit_for("bash")
        .saturating_mul(2)
        .max(16_384);
    let prefix_lines = preview_limit / 2 - 5;
    let command = format!(
        "python3 -c 'import sys; sys.stdout.write(\"a\\n\"*{prefix_lines}+\"password=synthetic-sensitive-secret-\"+\"x\"*64+\"\\nSTDOUT_SAFE_END\\n\"); sys.stderr.write(\"b\\n\"*{prefix_lines}+\"api_key=synthetic-sensitive-key-\"+\"z\"*64+\"\\nSTDERR_SAFE_END\\n\")'"
    );
    let large = call(&executor, "bash", json!({"command":command})).await;
    assert!(!large.is_error, "{large:?}");
    assert!(
        large.output.contains("additional output is unavailable"),
        "{large:?}"
    );
    assert!(large.output.contains("STDOUT_SAFE_END"), "{large:?}");
    assert!(large.output.contains("STDERR_SAFE_END"), "{large:?}");
    for fragment in ["sensitive-secret", "sensitive-key"] {
        assert!(!large.output.contains(fragment), "{large:?}");
    }
    assert!(large.output.len() < 10_000);

    for size in [preview_limit, preview_limit + 1] {
        let command = format!(
            "python3 -c 'import sys; sys.stdout.write(\"x\"*{size}); sys.stderr.write(\"stderr-ok\\n\")'"
        );
        let output = call(&executor, "bash", json!({"command":command})).await;
        assert!(!output.is_error, "{output:?}");
        assert!(output.output.contains("xxxx"), "{output:?}");
        assert!(output.output.contains("stderr-ok"), "{output:?}");
        assert!(
            output.output.contains("additional output is unavailable"),
            "{output:?}"
        );
    }

    for args in [
        json!({"command":"touch must-not-run", "stdin":true}),
        json!({"command":"touch must-not-run", "yield_time_ms":0}),
        json!({"command":"touch must-not-run", "yield_time_ms":10}),
    ] {
        let rejected = call(&executor, "bash", args).await;
        assert!(rejected.is_error, "{rejected:?}");
        assert_eq!(
            rejected.tool_result_fields.as_ref().unwrap()["execution_started"],
            false
        );
        assert!(!workspace.path().join("must-not-run").exists());
    }
}

#[tokio::test]
async fn foreground_only_executor_does_not_create_unread_output_capture() {
    let workspace = tempfile::tempdir().unwrap();
    let project = workspace.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let output_root = workspace.path().join("captures");
    let mut executor = ToolExecutor::new(&project);
    executor.background_tasks =
        crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(output_root.clone());
    let result = call(
        &executor,
        "bash",
        json!({"command":"printf ok", "stdin":false}),
    )
    .await;
    assert!(!result.is_error && result.output == "ok", "{result:?}");
    assert!(!output_root.exists());
}

#[tokio::test]
async fn foreground_timeout_preserves_redacted_unterminated_diagnostics() {
    let workspace = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(workspace.path());
    let result = call(&executor, "bash", json!({
        "command": "printf 'partial stdout\\377'; printf 'diagnostic password=secret-value' >&2; sleep 3",
        "timeout": 0.5
    })).await;
    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("partial stdout"), "{result:?}");
    assert!(result.output.contains("non-UTF-8"), "{result:?}");
    assert!(result.output.contains("diagnostic password="), "{result:?}");
    assert!(!result.output.contains("secret-value"), "{result:?}");
    assert!(
        result.output.contains("additional output is unavailable"),
        "{result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn capture_directory_inside_workspace_falls_back_without_writing_there() {
    let (workspace, _) = fixture();
    let link = workspace.root.path().join("project-link");
    std::os::unix::fs::symlink(workspace.path(), &link).unwrap();
    for output_root in [workspace.path().join("captures"), link.join("captures")] {
        let registry =
            crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(output_root.clone());
        let executor = ToolExecutor::new(workspace.path()).with_background_tasks(registry);
        let result = call(&executor, "bash", json!({"command":"printf ok"})).await;
        assert!(!result.is_error && result.output == "ok", "{result:?}");
        assert!(!output_root.exists());
        let rejected = call(
            &executor,
            "bash",
            json!({"command":"touch must-not-run", "yield_time_ms":0}),
        )
        .await;
        assert!(rejected.is_error, "{rejected:?}");
        assert!(!workspace.path().join("must-not-run").exists());
    }
}

#[tokio::test]
async fn restored_background_result_reports_unavailable_output() {
    let (workspace, executor) = fixture();
    let id = launch(&executor, "seq 1 20000", false, 5.0).await;
    terminal(&executor, &id).await;
    let mut registry = executor.background_tasks.clone();
    let projections = registry.export_shell_task_projections();
    drop(registry);
    drop(executor);
    let mut restored = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        workspace.path().join("restored"),
    );
    restored
        .restore_shell_task_projections(projections)
        .unwrap();
    assert!(
        restored
            .get_combined_output_since_async(&id, 0, 64)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn restored_task_wait_distinguishes_completion_from_lost_ownership() {
    let workspace = tempfile::tempdir().unwrap();
    let stdout = workspace.path().join("stdout");
    let stderr = workspace.path().join("stderr");
    std::fs::write(&stdout, "recorded output").unwrap();
    std::fs::write(&stderr, "").unwrap();
    let mut registry = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        workspace.path().join("restored"),
    );
    for (id, status) in [
        ("restored-complete", "completed"),
        ("restored-live", "running"),
    ] {
        registry
            .restore_shell_task_projection(
                astra_services::session_workspace::BackgroundShellTaskProjection {
                    terminal_facts: None,
                    id: id.into(),
                    status: status.into(),
                    title: "restored shell".into(),
                    started_at_ms: 1,
                    ended_at_ms: (status == "completed").then_some(2),
                    stdout_path: stdout.display().to_string(),
                    stderr_path: stderr.display().to_string(),
                    exit_code: (status == "completed").then_some(0),
                    terminal_reason: None,
                },
            )
            .unwrap();
    }
    let executor = ToolExecutor::new(workspace.path()).with_background_tasks(registry);
    let done = call(
        &executor,
        "task_output",
        json!({"task_id":"restored-complete","block":true}),
    )
    .await;
    assert!(
        !done.is_error && done.output.contains("recorded output"),
        "{done:?}"
    );
    let stale = call(
        &executor,
        "task_output",
        json!({"task_id":"restored-live","block":true}),
    )
    .await;
    assert!(stale.output.contains("stale_handle"), "{stale:?}");
    let unknown = call(
        &executor,
        "task_output",
        json!({"task_id":"missing","block":true,"wake_on_output":true}),
    )
    .await;
    assert!(unknown.output.contains("not_found"), "{unknown:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn task_cleanup_uses_original_directory_after_path_replacement() {
    use std::os::unix::fs::symlink;
    for prune in [false, true] {
        let (workspace, executor) = fixture();
        let mut ids = Vec::new();
        let mut first_paths = None;
        for _ in 0..if prune { 3 } else { 1 } {
            let id = launch(&executor, "printf task", false, 5.0).await;
            terminal(&executor, &id).await;
            if first_paths.is_none() {
                let task = executor.background_tasks.get(&id).unwrap();
                first_paths = Some((task.stdout_path, task.stderr_path));
            }
            ids.push(id);
        }
        let last = if prune {
            Some(
                launch(
                    &executor,
                    "while [ ! -f release ]; do sleep 0.01; done; printf task",
                    false,
                    5.0,
                )
                .await,
            )
        } else {
            None
        };
        let (stdout, stderr) = first_paths.unwrap();
        let original_dir = stdout.parent().unwrap().to_path_buf();
        let moved = original_dir.with_file_name("moved-captures");
        std::fs::rename(&original_dir, &moved).unwrap();
        let victim = workspace.path().join("user-files");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join(stdout.file_name().unwrap()), b"user stdout").unwrap();
        std::fs::write(victim.join(stderr.file_name().unwrap()), b"user stderr").unwrap();
        symlink(&victim, &original_dir).unwrap();
        let registry = executor.background_tasks.clone();
        if let Some(last) = last {
            std::fs::write(workspace.path().join("release"), b"").unwrap();
            terminal(&executor, &last).await;
            recorded(&executor, &last).await;
            assert!(registry.get(&ids[0]).is_none());
        }
        drop(registry);
        drop(executor);
        assert_eq!(
            std::fs::read(victim.join(stdout.file_name().unwrap())).unwrap(),
            b"user stdout"
        );
        assert_eq!(
            std::fs::read(victim.join(stderr.file_name().unwrap())).unwrap(),
            b"user stderr"
        );
        assert!(!moved.join(stdout.file_name().unwrap()).exists());
        assert!(!moved.join(stderr.file_name().unwrap()).exists());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn retained_tasks_share_directory_descriptor_and_survive_rebind() {
    fn directory_fd_count(path: &std::path::Path) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| target == path)
            .count()
    }
    let (workspace, executor) = fixture();
    let mut paths = Vec::new();
    for _ in 0..4 {
        let id = launch(&executor, "printf task", false, 5.0).await;
        terminal(&executor, &id).await;
        paths.push(executor.background_tasks.get(&id).unwrap().stdout_path);
    }
    let old_directory = paths[0].parent().unwrap().to_path_buf();
    assert_eq!(
        directory_fd_count(&old_directory),
        1,
        "one pinned directory fd per namespace"
    );
    let mut registry = executor.background_tasks.clone();
    registry.rebind_output_dir_for_new_tasks(workspace.root.path().join("new-captures"));
    let id = launch(&executor, "printf new", false, 5.0).await;
    terminal(&executor, &id).await;
    paths.push(registry.get(&id).unwrap().stdout_path);
    assert_ne!(
        paths.last().unwrap().parent(),
        Some(old_directory.as_path())
    );
    drop(registry);
    drop(executor);
    assert!(
        paths.iter().all(|path| !path.exists()),
        "both namespaces must be reclaimed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failed_eviction_retains_cleanup_owner_for_retry() {
    let (_workspace, executor) = fixture();
    let first = launch(&executor, "printf task", false, 5.0).await;
    terminal(&executor, &first).await;
    let stdout = executor.background_tasks.get(&first).unwrap().stdout_path;
    let moved = stdout.with_extension("moved");
    std::fs::rename(&stdout, &moved).unwrap();
    std::fs::write(&stdout, b"user file").unwrap();
    let mut last = String::new();
    for _ in 0..3 {
        let id = launch(&executor, "printf task", false, 5.0).await;
        terminal(&executor, &id).await;
        last = id;
    }
    recorded(&executor, &last).await;
    let mut registry = executor.background_tasks.clone();
    assert!(
        registry.get(&first).is_some(),
        "failed cleanup must retain its owner"
    );
    assert_eq!(std::fs::read(&stdout).unwrap(), b"user file");
    std::fs::remove_file(&stdout).unwrap();
    std::fs::rename(moved, &stdout).unwrap();
    for _ in 0..10 {
        registry.poll_completions();
    }
    assert!(
        registry.get(&first).is_some(),
        "idle UI polls must not retry cleanup"
    );
    registry.prune_retained_terminal_tasks();
    assert!(
        registry.get(&first).is_none(),
        "cleanup should succeed on retry"
    );
    assert!(!stdout.exists());
}

#[tokio::test]
async fn task_execution_headless_input_preserves_bytes_and_does_not_claim_completion() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "printf 'pid:%s\\n' $$; IFS= read -r value; printf '<%s>\\n' \"$value\"; cat; printf 'finished\\n'", true, 5.0).await;
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"  hello  \n"}),
    )
    .await;
    assert!(!input.is_error, "{input:?}");
    let fields = input.tool_result_fields.unwrap();
    assert_eq!(fields["input_delivery"], "written");
    assert_eq!(fields["terminal"], false);
    assert_eq!(fields["task"]["status"], "running");
    let closed = call(
        &executor,
        "task_input",
        json!({"task_id":id,"close_stdin":true}),
    )
    .await;
    assert!(!closed.is_error, "{closed:?}");
    let outcome = terminal(&executor, &id).await;
    assert!(!outcome.is_error, "{outcome:?}");
    assert!(outcome.output.contains("<  hello  >"));
    assert!(outcome.output.contains("finished"));
    assert_eq!(outcome.tool_result_fields.as_ref().unwrap()["task_id"], id);
    let rejected = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"duplicate"}),
    )
    .await;
    assert!(rejected.is_error);
    assert_eq!(
        rejected.tool_result_fields.unwrap()["input_delivery"],
        "not_sent"
    );
}

#[tokio::test]
async fn task_execution_default_stdin_is_closed_and_unsupported_input_is_explicit() {
    let (_root, executor) = fixture();
    let outcome = call(&executor, "bash", json!({"command":"if read -r value; then printf unexpected; else printf eof; fi","timeout":5})).await;
    assert!(!outcome.is_error, "{outcome:?}");
    assert!(outcome.output.contains("eof"));
    let id = launch(&executor, "sleep 10", false, 15.0).await;
    let input = call(
        &executor,
        "task_input",
        json!({"task_id":id,"input":"no implicit terminal input"}),
    )
    .await;
    assert!(input.is_error);
    assert_eq!(
        input.tool_result_fields.unwrap()["input_delivery"],
        "not_sent"
    );
    let stopped = call(&executor, "task_stop", json!({"task_id":id})).await;
    assert!(!stopped.is_error, "{stopped:?}");
    terminal(&executor, &id).await;
}

#[tokio::test]
async fn task_execution_cancelled_observer_leaves_the_process_controllable() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "cat", true, 5.0).await;
    stdin_ready(&executor, &id).await;
    let initial = call(&executor, "task_output", json!({"task_id":id})).await;
    let revision = initial.tool_result_fields.as_ref().unwrap()["background_task_observation"]
        ["output_revision"].as_u64().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let watcher = {
        let (executor, id, token) = (executor.clone(), id.clone(), cancel.clone());
        tokio::spawn(async move {
            executor
                .execute_with_invocation_metadata_cancelable(
                    "task_output",
                    &json!({"task_id":id,"block":true,"wake_on_output":true,
                        "after_output_revision":revision,"timeout_ms":30000}),
                    ToolInvocationMetadata::default(),
                    Some(&token),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !executor.foreground_wait_control.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(watcher.await.unwrap().is_error);
    if executor
        .background_tasks
        .get(&id)
        .unwrap()
        .status()
        .is_terminal()
    {
        panic!(
            "observer unexpectedly saw terminal: {:?}",
            terminal(&executor, &id).await
        );
    }
    assert!(
        !call(
            &executor,
            "task_input",
            json!({"task_id":id,"input":"still useful\n"})
        )
        .await
        .is_error
    );
    assert!(
        !call(
            &executor,
            "task_input",
            json!({"task_id":id,"close_stdin":true})
        )
        .await
        .is_error
    );
    let outcome = terminal(&executor, &id).await;
    assert!(!outcome.is_error, "{outcome:?}");
    assert!(outcome.output.contains("still useful"), "{outcome:?}");
}

#[tokio::test]
async fn task_execution_release_keeps_one_task_and_original_deadline() {
    let (_root, executor) = fixture();
    let control = executor.foreground_wait_control.clone();
    let run = {
        let executor = executor.clone();
        tokio::spawn(async move {
            call(&executor, "bash", json!({"command":"printf 'same-pid:%s\\n' $$; sleep 10","timeout":0.5,"yield_time_ms":30000})).await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !control.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(control.release());
    let background = run.await.unwrap();
    assert!(!background.is_error, "{background:?}");
    let id = background.tool_result_fields.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(executor.background_tasks.all_tasks().count(), 1);
    let outcome = terminal(&executor, &id).await;
    assert!(
        outcome.is_error,
        "promotion must not reset its physical deadline"
    );
    assert_eq!(
        outcome.tool_result_fields.as_ref().unwrap()["error_kind"],
        "tool_timeout"
    );
    assert_eq!(outcome.tool_result_fields.as_ref().unwrap()["task_id"], id);
    assert!(outcome.output.contains("same-pid:"), "{outcome:?}");
    assert!(
        outcome
            .tool_result_fields
            .as_ref()
            .unwrap()
            .get("process_id")
            .is_none()
    );
}

#[tokio::test]
async fn task_execution_release_keeps_the_running_shell() {
    let (root, executor) = fixture();
    let control = executor.foreground_wait_control.clone();
    let run = {
        let executor = executor.clone();
        tokio::spawn(async move {
            call(&executor, "bash", json!({
                "command": "printf '%s' $$ > before.pid; IFS= read -r input; printf '%s' $$ > after.pid",
                "stdin": true, "timeout": 10, "yield_time_ms": 30000
            })).await
        })
    };
    let before = root.path().join("before.pid");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !control.is_active()
            || !std::fs::read_to_string(&before).is_ok_and(|pid| !pid.is_empty())
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("shell started while foreground wait was active");
    let before_pid = std::fs::read_to_string(before).unwrap();
    assert!(!before_pid.is_empty());
    assert!(control.release());
    let background = run.await.unwrap();
    assert!(!background.is_error, "{background:?}");
    let id = background.tool_result_fields.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    stdin_ready(&executor, &id).await;
    assert!(
        !call(
            &executor,
            "task_input",
            json!({"task_id":id,"input":"continue\n"})
        )
        .await
        .is_error
    );
    let outcome = terminal(&executor, &id).await;
    assert!(!outcome.is_error, "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("after.pid")).unwrap(),
        before_pid
    );
    assert!(
        outcome
            .tool_result_fields
            .as_ref()
            .unwrap()
            .get("process_id")
            .is_none()
    );
}

#[tokio::test]
async fn task_execution_repeated_observations_and_bounded_wait_remain_available() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "cat", true, 5.0).await;
    stdin_ready(&executor, &id).await;
    for _ in 0..3 {
        let output = call(&executor, "task_output", json!({"task_id":id})).await;
        assert!(!output.is_error, "{output:?}");
        assert_eq!(
            output.tool_result_fields.unwrap()["task"]["status"],
            "running"
        );
    }
    let output = call(
        &executor,
        "task_output",
        json!({"task_id":id,"block":true,"timeout_ms":1}),
    )
    .await;
    assert!(!output.is_error);
    assert_eq!(
        output.tool_result_fields.as_ref().unwrap()["wait_result"],
        "timeout"
    );
    if executor
        .background_tasks
        .get(&id)
        .unwrap()
        .status()
        .is_terminal()
    {
        panic!(
            "observer unexpectedly saw terminal: {:?}",
            terminal(&executor, &id).await
        );
    }
    call(
        &executor,
        "task_input",
        json!({"task_id":id,"close_stdin":true}),
    )
    .await;
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn task_execution_stop_is_a_request_until_the_owner_observes_terminal() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "cat", true, 5.0).await;
    stdin_ready(&executor, &id).await;
    let stopped = call(&executor, "task_stop", json!({"task_id":id})).await;
    assert!(!stopped.is_error, "{stopped:?}");
    let result: Value = serde_json::from_str(&stopped.output).unwrap();
    assert_eq!(result["status"], "stop_requested");
    assert_eq!(result["terminal"], false);
    let outcome = terminal(&executor, &id).await;
    assert!(outcome.is_error);
    assert_eq!(
        outcome.tool_result_fields.as_ref().unwrap()["error_kind"],
        "cancelled"
    );
    assert_eq!(
        outcome.tool_result_fields.as_ref().unwrap()["bash_output_capture"]["complete"],
        false
    );
}

#[tokio::test]
async fn task_observation_cancellation_does_not_claim_parent_or_task_stopped() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "cat", true, 5.0).await;
    stdin_ready(&executor, &id).await;
    let parent = tokio_util::sync::CancellationToken::new();
    let observer = parent.child_token();
    let wait = {
        let (executor, id, observer) = (executor.clone(), id.clone(), observer.clone());
        tokio::spawn(async move {
            executor
                .execute_with_invocation_metadata_cancelable(
                    "task_output",
                    &json!({"task_id":id,"block":true,"timeout_ms":300_000}),
                    ToolInvocationMetadata::default(),
                    Some(&observer),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while !executor.foreground_wait_control.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    observer.cancel();
    let outcome = wait.await.unwrap();
    assert!(outcome.is_error);
    let fields = outcome.tool_result_fields.unwrap();
    assert_eq!(fields["error_kind"], "cancelled");
    assert_eq!(fields["execution_started"], true);
    assert_eq!(fields["stage"], "observation");
    assert_eq!(fields["side_effects_maybe"], false);
    assert!(!parent.is_cancelled());
    assert!(!outcome.output.contains("stop_or_resume_parent_turn"));
    assert!(!outcome.output.contains("not executed"));
    let status = call(
        &executor,
        "task_output",
        json!({"task_id":id,"block":false}),
    )
    .await;
    assert!(!status.is_error, "{status:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&status.output).unwrap()["status"],
        "running"
    );
    call(
        &executor,
        "task_input",
        json!({"task_id":id,"close_stdin":true}),
    )
    .await;
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn task_observation_keeps_admitted_cap_and_absolute_cutoff() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "cat", true, 5.0).await;
    stdin_ready(&executor, &id).await;
    for (cutoff, cap, wake) in [
        (50u64, 300_000u64, false),
        (5_000, 25, false),
        (50, 300_000, true),
        (5_000, 25, true),
    ] {
        let started = std::time::Instant::now();
        let outcome = executor
            .execute_with_invocation_metadata_cancelable(
                "task_output",
                &json!({"task_id":id,"block":true,"timeout_ms":300_000,"wake_on_output":wake}),
                ToolInvocationMetadata {
                    observation_deadline: Some(started + Duration::from_millis(cutoff)),
                    command_timeout_cap_ms: Some(cap),
                    ..Default::default()
                },
                None,
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(1), "{outcome:?}");
        let fields = outcome.tool_result_fields.as_ref().unwrap();
        if outcome.is_error {
            assert_eq!(fields["error_kind"], "tool_timeout");
            assert_eq!(fields["stage"], "observation");
            assert_eq!(fields["execution_started"], true);
            assert_eq!(fields["side_effects_maybe"], false);
        } else {
            assert_eq!(fields["wait_result"], "timeout");
            assert!(outcome.output.contains("Observation deadline reached"));
            assert!(!outcome.output.contains("300000"));
        }
    }
    let cutoff = std::time::Instant::now() + Duration::from_millis(10);
    let invocation = ToolInvocationMetadata {
        observation_deadline: Some(cutoff),
        ..Default::default()
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    let rejected = executor
        .execute_with_invocation_metadata_cancelable(
            "task_output",
            &json!({"task_id":id,"block":true,"timeout_ms":300_000}),
            invocation,
            None,
        )
        .await;
    assert!(rejected.is_error);
    let fields = rejected.tool_result_fields.unwrap();
    assert_eq!(fields["execution_started"], false);
    assert_eq!(fields["error_kind"], "tool_timeout");
    let running = call(
        &executor,
        "task_output",
        json!({"task_id":id,"block":false}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&running.output).unwrap()["status"],
        "running"
    );
    call(
        &executor,
        "task_input",
        json!({"task_id":id,"close_stdin":true}),
    )
    .await;
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn task_execution_parent_guidance_releases_wait_without_timeout_or_input() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "cat", true, 5.0).await;
    stdin_ready(&executor, &id).await;
    let wait = {
        let (executor, id) = (executor.clone(), id.clone());
        tokio::spawn(async move {
            call(
                &executor,
                "task_output",
                json!({"task_id":id,"block":true,"timeout_ms":30000}),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while !executor.foreground_wait_control.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    executor.foreground_wait_control.release();
    let observed = wait.await.unwrap();
    assert!(!observed.is_error);
    let fields = observed.tool_result_fields.unwrap();
    assert_eq!(fields["wait_result"], "released");
    assert_eq!(fields["task"]["status"], "running");
    assert!(!observed.output.contains("timed out"));
    call(
        &executor,
        "task_input",
        json!({"task_id":id,"close_stdin":true}),
    )
    .await;
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn task_execution_plan_authoring_denies_writes_and_child_input_at_public_entry() {
    let (_root, executor) = fixture();
    executor.set_active_session_id("plan-session");
    let id = launch(&executor, "cat", true, 5.0).await;
    stdin_ready(&executor, &id).await;
    executor
        .set_plan_mode_authoring_cache_for_active_session(true)
        .await;
    for (tool, args) in [
        ("bash", json!({"command":"touch should-not-exist"})),
        (
            "task_input",
            json!({"task_id":id,"input":"not authorized\n"}),
        ),
    ] {
        let rejected = call(&executor, tool, args).await;
        assert!(rejected.is_error, "{rejected:?}");
        assert_eq!(
            rejected.tool_result_fields.unwrap()["execution_started"],
            false
        );
    }
    assert!(
        !executor
            .effective_project_root()
            .join("should-not-exist")
            .exists()
    );
    assert!(
        !call(&executor, "task_output", json!({"task_id":id}))
            .await
            .is_error
    );
    executor
        .set_plan_mode_authoring_cache_for_active_session(false)
        .await;
    call(
        &executor,
        "task_input",
        json!({"task_id":id,"close_stdin":true}),
    )
    .await;
    assert!(!terminal(&executor, &id).await.is_error);
}

#[tokio::test]
async fn task_execution_background_task_retains_launch_cancellation() {
    let (_root, executor) = fixture();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let outcome = executor
        .execute_with_invocation_metadata_cancelable(
            "bash",
            &json!({"command":"cat","stdin":true,"yield_time_ms":0,"timeout":10}),
            ToolInvocationMetadata::default(),
            Some(&cancellation),
        )
        .await;
    let id = outcome.tool_result_fields.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    stdin_ready(&executor, &id).await;
    cancellation.cancel();
    let outcome = terminal(&executor, &id).await;
    assert!(outcome.is_error);
    assert_eq!(
        outcome.tool_result_fields.as_ref().unwrap()["error_kind"],
        "cancelled"
    );
}

#[tokio::test]
async fn task_execution_thousand_public_waiters_share_one_capture_projection() {
    let (root, executor) = fixture();
    std::fs::write(root.path().join("bulk-output"), "output!\n".repeat(131_072)).unwrap();
    let id = launch(
        &executor,
        "cat bulk-output; read -r line; printf 'done\n'",
        true,
        15.0,
    )
    .await;
    stdin_ready(&executor, &id).await;
    let started = std::time::Instant::now();
    let mut waiters = tokio::task::JoinSet::new();
    let ready = Arc::new(tokio::sync::Semaphore::new(0));
    for _ in 0..1_000 {
        let (executor, id) = (executor.clone(), id.clone());
        let ready = ready.clone();
        waiters.spawn(async move {
            let response = call(
                &executor,
                "task_output",
                json!({"task_id":id,"block":true,"timeout_ms":10000,"max_bytes":128}),
            );
            tokio::pin!(response);
            // Poll the actual admitted public call to a pending wait, rather
            // than merely queueing 1,000 unpolled tasks before child release.
            assert!(futures_util::poll!(response.as_mut()).is_pending());
            ready.add_permits(1);
            let result = response.await;
            assert!(!result.is_error, "{result:?}");
            assert!(result.output.len() < 4096);
            assert_eq!(
                result.tool_result_fields.as_ref().unwrap()["task"]["status"],
                "completed"
            );
        });
    }
    tokio::time::timeout(Duration::from_secs(10), ready.acquire_many(1_000))
        .await
        .expect("all public waits reached Pending")
        .unwrap()
        .forget();
    call(
        &executor,
        "task_input",
        json!({"task_id":id,"close_stdin":true}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(result) = waiters.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("all public waiters settle");
    println!(
        "1000 public tool waiters, 1 physical process, 1 MiB capture, bounded 128-byte reads: {:?}; no database/provider operations",
        started.elapsed()
    );
}
