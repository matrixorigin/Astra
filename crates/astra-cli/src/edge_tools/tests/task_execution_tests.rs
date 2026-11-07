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
            if output
                .output
                .split_once("Output chunk:\n")
                .is_some_and(|(_, chunk)| chunk.contains("Confirm?") && chunk.contains("warning"))
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
        !observed.is_error
            && observed
                .output
                .split_once("Output chunk:\n")
                .is_some_and(|(_, chunk)| chunk.contains("Password:")),
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
        !second.is_error
            && second
                .output
                .split_once("Output chunk:\n")
                .is_some_and(|(_, chunk)| chunk.contains("Password:")),
        "{second:?}"
    );
    let again = call(
        &executor,
        "task_output",
        json!({"task_id":id,"block":true,"wake_on_output":true,"timeout_ms":1000}),
    )
    .await;
    assert!(
        !again.is_error
            && again
                .output
                .split_once("Output chunk:\n")
                .is_some_and(|(_, chunk)| chunk.contains("Password:")),
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
        assert!(!output.is_error && output.output.split_once("Output chunk:\n").is_some_and(|(_, chunk)| chunk.contains("READY:") && chunk.contains("warning")), "{output:?}");
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
async fn an_unavailable_stream_does_not_hide_the_other_stream() {
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
        let broken_label = if broken_stdout { "stdout" } else { "stderr" };
        assert!(
            !output.is_error
                && output.output.contains("READY:")
                && output
                    .output
                    .contains(&format!("{broken_label} unavailable")),
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
                && search.output.contains("search results are incomplete"),
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
        !output.is_error
            && output
                .output
                .split_once("Output chunk:\n")
                .is_some_and(|(_, chunk)| chunk.contains("failure-detail")),
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
async fn published_command_output_survives_task_retention() {
    let (workspace, executor) = fixture();
    let result = call(&executor, "bash", json!({"command":"seq 1 20000"})).await;
    assert!(!result.is_error, "{result:?}");
    let path = result.tool_result_fields.as_ref().unwrap()["bash_output_capture"]["stdout"]["path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(std::path::Path::new(&path).starts_with(workspace.path().join("astra-bash-outputs")));
    for _ in 0..8 {
        let next = call(&executor, "bash", json!({"command":"printf ok"})).await;
        assert!(!next.is_error, "{next:?}");
    }
    let bytes = std::fs::read(&path).expect("published workspace output must not be evicted");
    assert!(bytes.starts_with(b"1\n2\n"));
    assert!(bytes.ends_with(b"20000\n"));
    let reread = call(
        &executor,
        "read_file",
        json!({"path":path,"start_line":19999,"end_line":20000}),
    )
    .await;
    assert!(
        !reread.is_error && reread.output.contains("20000"),
        "{reread:?}"
    );
    let id = launch(&executor, "printf background", false, 5.0).await;
    terminal(&executor, &id).await;
    let private = executor.background_tasks.get(&id).unwrap().stdout_path;
    assert!(private.exists());
    drop(executor);
    assert!(
        !private.exists(),
        "session close must reclaim its private capture"
    );
    assert!(
        std::path::Path::new(&path).exists(),
        "session close must retain published workspace output"
    );
}

#[tokio::test]
async fn restored_background_results_remain_readable_across_repeated_restore() {
    let (workspace, executor) = fixture();
    let large_id = launch(&executor, "seq 1 20000", false, 5.0).await;
    let large = terminal(&executor, &large_id).await;
    let published =
        large.tool_result_fields.as_ref().unwrap()["bash_output_capture"]["stdout"]["path"]
            .as_str()
            .unwrap()
            .to_owned();
    let short_id = launch(&executor, "printf short", false, 5.0).await;
    terminal(&executor, &short_id).await;
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
            .get(&large_id)
            .unwrap()
            .output_reference()
            .contains(&published)
    );
    assert!(
        restored
            .get_combined_output_since_async(&large_id, 0, 64)
            .await
            .unwrap()
            .0
            .contains("1\n2\n")
    );
    assert!(
        restored
            .get_combined_output_since_async(&short_id, 0, 64)
            .await
            .is_err()
    );
    assert!(
        restored
            .search_combined_output_async(&short_id, "short", 0, 64)
            .await
            .is_err()
    );
    let projections = restored.export_shell_task_projections();
    drop(restored);
    let mut again = crate::edge_tools::background_tasks::BackgroundTaskRegistry::new(
        workspace.path().join("again"),
    );
    again.restore_shell_task_projections(projections).unwrap();
    assert!(
        again
            .get(&large_id)
            .unwrap()
            .output_reference()
            .contains(&published)
    );
    assert!(
        again
            .get_combined_output_since_async(&large_id, 0, 64)
            .await
            .unwrap()
            .0
            .contains("1\n2\n")
    );
    drop(again);
    assert!(std::path::Path::new(&published).exists());
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
    registry.rebind_output_dir_for_new_tasks(workspace.path().join("new-captures"));
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
