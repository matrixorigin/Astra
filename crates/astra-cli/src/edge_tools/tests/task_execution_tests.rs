//! Public executor journeys, independent of a TUI command-draining loop.
use super::*;
use astra_tools::tool_engine::ToolInvocationMetadata;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn fixture() -> (tempfile::TempDir, Arc<ToolExecutor>) {
    let root = tempfile::tempdir().unwrap();
    let tasks = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        root.path().join("captures"),
    );
    let executor = Arc::new(ToolExecutor::new(root.path()).with_background_tasks(tasks));
    (root, executor)
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
            if task
                .input
                .get()
                .is_some_and(astra_sandbox::ProcessInput::is_open)
            {
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
async fn task_execution_headless_input_preserves_bytes_and_does_not_claim_completion() {
    let (_root, executor) = fixture();
    let id = launch(&executor, "printf 'pid:%s\\n' $$; IFS= read -r value; printf '<%s>\\n' \"$value\"; cat; printf 'finished\\n'", true, 5.0).await;
    stdin_ready(&executor, &id).await;
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
    let cancel = tokio_util::sync::CancellationToken::new();
    let watcher = {
        let (executor, id, token) = (executor.clone(), id.clone(), cancel.clone());
        tokio::spawn(async move {
            executor
                .execute_with_invocation_metadata_cancelable(
                    "task_output",
                    &json!({"task_id":id,"block":true,"timeout_ms":30000}),
                    ToolInvocationMetadata::default(),
                    Some(&token),
                )
                .await
        })
    };
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
async fn task_execution_release_keeps_one_task_pid_and_original_deadline() {
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
    let pid = outcome.tool_result_fields.as_ref().unwrap()["process_id"]
        .as_u64()
        .unwrap();
    assert!(
        outcome.output.contains(&format!("same-pid:{pid}")),
        "{outcome:?}"
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
