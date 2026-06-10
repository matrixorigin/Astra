use crate::cli::arg_render::join_words;
use crate::cli::cli_config::cli_args::TaskResultArgs;
use crate::cli::cli_config::cli_utils::cli_user_id;
use crate::cli::exit_code::ExitCode;
use crate::cli::session::session_runtime;
use crate::cli::slash::slash_task;
use crate::cli::stream::streaming_types::StreamResult;
use crate::cli::surface::task_result_surface::{
    load_task_result_read_surface, render_task_result_header_value, task_result_header_fields,
    task_result_json_payload,
};
use crate::cli::task::task_result_artifact::load_task_result_artifact;
use crate::cli::task::task_result_projection::{
    stream_result_completion_outcome, stream_result_exit_code, stream_result_failure_reason,
    task_checkpoint_state_from_result,
};
use crossterm::style::Stylize;

pub(crate) async fn resolve_task_result_task_id(
    svc: &dyn astra_services::TaskService,
    query: &str,
) -> Result<String, String> {
    let user_id = cli_user_id();
    slash_task::find_task_by_query(svc, &user_id, query)
        .await?
        .ok_or_else(|| format!("no task matching '{query}'"))
}

pub(crate) async fn finalize_headless_task_result<T: astra_services::TaskService + ?Sized>(
    svc: &T,
    task_id: &str,
    sr: &StreamResult,
    task_session_id: Option<&str>,
    output_path: Option<&str>,
) -> Result<ExitCode, String> {
    let exit_code = stream_result_exit_code(sr);
    let state = task_checkpoint_state_from_result(sr, output_path, exit_code);
    svc.save_checkpoint(
        task_id,
        &astra_services::TaskCheckpoint {
            active_subtask_id: None,
            turn: 0,
            session_id: task_session_id
                .map(str::to_string)
                .or_else(|| sr.session_id.clone()),
            state,
        },
    )
    .await?;

    match exit_code {
        ExitCode::Success => {
            svc.complete_task(task_id).await?;
        }
        ExitCode::Partial => {
            svc.complete_task_with_outcome(task_id, stream_result_completion_outcome(sr))
                .await?;
        }
        ExitCode::Unfinished => {
            unreachable!("unfinished exit code is only valid for task result lookup")
        }
        ExitCode::ToolFailure
        | ExitCode::ForceStop
        | ExitCode::ApiError
        | ExitCode::PersistenceError => {
            let failure_reason = stream_result_failure_reason(exit_code, sr);
            svc.fail_task(task_id, &failure_reason).await?;
        }
    }

    Ok(exit_code)
}

pub(crate) async fn execute_task_result(args: TaskResultArgs) -> Result<ExitCode, String> {
    let query = join_words(&args.query);
    if query.trim().is_empty() {
        return Err("provide a task id or title fragment".to_string());
    }

    // No profile in this CLI subcommand context; HttpTaskService
    // falls back to env-only token resolution which is fine for
    // one-shot `astra task result <query>` invocations.
    let svc = session_runtime::resolve_task_service(None).await;
    let task_id = resolve_task_result_task_id(&*svc, &query).await?;
    let task = svc
        .get_task(&task_id)
        .await?
        .ok_or_else(|| format!("task disappeared: {task_id}"))?;
    let read = load_task_result_read_surface(&task);

    let short = &task.task_id[..8.min(task.task_id.len())];
    eprintln!(
        "\n{}",
        format!("─── Task Result ({short}) ─────────────────────────").bold()
    );
    eprintln!("  {:<12} {}", "title:".dim(), task.title);
    for field in task_result_header_fields(&read) {
        eprintln!(
            "  {:<12} {}",
            field.label.dim(),
            render_task_result_header_value(&field)
        );
    }

    let artifact = load_task_result_artifact(&task)?;
    if args.json {
        let artifact_surface = artifact.as_ref().map(|artifact| artifact.as_surface());
        println!(
            "{}",
            serde_json::to_string_pretty(&task_result_json_payload(&read, artifact_surface))
                .unwrap_or_default()
        );
        eprintln!();
        return Ok(read.exit_code);
    }

    if let Some(artifact) = artifact {
        eprintln!();
        println!("{}", artifact.full_text);
        if let Some(tokens) = artifact.prompt_tokens {
            let comp = artifact.completion_tokens;
            let tools = artifact.tool_calls_count;
            eprintln!(
                "\n  {}",
                format!("tokens: {tokens}→/{comp}← | tools: {tools}").dim()
            );
        }
        if let Some(output_file) = artifact.output_file {
            eprintln!("  {}", format!("output: {output_file}").dim());
        }
        eprintln!();
        return Ok(read.exit_code);
    }

    if read.header.is_unfinished {
        eprintln!("  {}", read.missing_text.yellow());
    } else {
        eprintln!("  {}", read.missing_text.dim());
        eprintln!();
        return Ok(read.exit_code);
    }
    eprintln!();
    Ok(read.exit_code)
}

#[cfg(test)]
mod tests {
    use super::{finalize_headless_task_result, resolve_task_result_task_id};
    use crate::cli::exit_code::ExitCode;
    use crate::cli::stream::streaming_types::StreamResult;
    use astra_services::TaskService as _;

    struct CliUserIdGuard {
        previous: Option<String>,
    }

    impl CliUserIdGuard {
        fn set(value: &str) -> Self {
            let previous = std::env::var("ASTRA_CLI_USER_ID").ok();
            unsafe {
                std::env::set_var("ASTRA_CLI_USER_ID", value);
            }
            Self { previous }
        }
    }

    impl Drop for CliUserIdGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = self.previous.as_deref() {
                    std::env::set_var("ASTRA_CLI_USER_ID", previous);
                } else {
                    std::env::remove_var("ASTRA_CLI_USER_ID");
                }
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_task_result_task_id_uses_cli_user_id_scope() {
        let _user = CliUserIdGuard::set("task-user");
        let tmp = tempfile::tempdir().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let expected = svc
            .create_task(
                "task-user",
                "sess-1",
                astra_services::TaskCreateRequest {
                    title: "Build auth".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.create_task(
            "other-user",
            "sess-2",
            astra_services::TaskCreateRequest {
                title: "Build auth".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let found = resolve_task_result_task_id(&svc, "Build auth")
            .await
            .unwrap();
        assert_eq!(found, expected);
    }

    fn stream_result_for_task_checkpoint() -> StreamResult {
        StreamResult {
            session_id: Some("session-1".to_string()),
            run_id: Some("run-1".to_string()),
            session_persistence_error: Some("failed to append one-shot journal events".into()),
            full_text: "hello".to_string(),
            prompt_tokens: 10,
            completion_tokens: 3,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
            tool_calls_count: 2,
            tools_used: vec!["bash".to_string()],
            background_agent_results: vec![("agent-1".into(), "done".into())],
            ..Default::default()
        }
    }

    async fn create_in_progress_task(svc: &astra_services::LocalTaskService) -> String {
        let tid = svc
            .create_task(
                "test-user",
                "fallback-session",
                astra_services::TaskCreateRequest {
                    title: "run: test".to_string(),
                    description: Some("test".to_string()),
                    plan: None,
                    parent_task_id: None,
                    project_type: None,
                    goal_pattern: None,
                },
            )
            .await
            .unwrap();
        svc.update_status(&tid, astra_services::TaskStatus::InProgress)
            .await
            .unwrap();
        tid
    }

    #[tokio::test]
    async fn finalize_headless_task_result_marks_task_failed_and_persists_checkpoint_on_persistence_error()
     {
        let tmp = tempfile::tempdir().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let tid = create_in_progress_task(&svc).await;

        let sr = stream_result_for_task_checkpoint();
        let exit_code = finalize_headless_task_result(
            &svc,
            &tid,
            &sr,
            Some("fallback-session"),
            Some("/tmp/out.txt"),
        )
        .await
        .unwrap();

        assert_eq!(exit_code, ExitCode::PersistenceError);

        let record = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(record.status, astra_services::TaskStatus::Failed);
        assert_eq!(
            record.error_message.as_deref(),
            Some("failed to append one-shot journal events")
        );
        let checkpoint = record.checkpoint.expect("checkpoint should be saved");
        assert_eq!(checkpoint.session_id.as_deref(), Some("fallback-session"));
        assert_eq!(
            checkpoint
                .state
                .get("persistence_error")
                .and_then(|v| v.as_str()),
            Some("failed to append one-shot journal events")
        );
    }

    #[tokio::test]
    async fn finalize_headless_task_result_marks_task_completed_on_clean_success() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let tid = create_in_progress_task(&svc).await;

        let mut sr = stream_result_for_task_checkpoint();
        sr.session_id = None;
        sr.session_persistence_error = None;

        let exit_code = finalize_headless_task_result(
            &svc,
            &tid,
            &sr,
            Some("fallback-session"),
            Some("/tmp/out.txt"),
        )
        .await
        .unwrap();

        assert_eq!(exit_code, ExitCode::Success);

        let record = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(record.status, astra_services::TaskStatus::Completed);
        assert_eq!(record.outcome, Some(astra_services::TaskOutcome::Success));
        assert_eq!(record.error_message, None);
        let checkpoint = record.checkpoint.expect("checkpoint should be saved");
        assert_eq!(checkpoint.session_id.as_deref(), Some("fallback-session"));
        assert!(checkpoint.state["persistence_error"].is_null());
    }

    #[tokio::test]
    async fn finalize_headless_task_result_marks_task_completed_with_partial_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let tid = create_in_progress_task(&svc).await;

        let mut sr = stream_result_for_task_checkpoint();
        sr.session_persistence_error = None;
        sr.final_state = "interrupted".into();
        sr.interruption_kind = Some("budget_exhausted".into());

        let exit_code = finalize_headless_task_result(
            &svc,
            &tid,
            &sr,
            Some("fallback-session"),
            Some("/tmp/out.txt"),
        )
        .await
        .unwrap();

        assert_eq!(exit_code, ExitCode::Partial);

        let record = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(record.status, astra_services::TaskStatus::Completed);
        assert_eq!(record.outcome, Some(astra_services::TaskOutcome::Partial));
        assert_eq!(record.error_message, None);
        let checkpoint = record.checkpoint.expect("checkpoint should be saved");
        assert_eq!(checkpoint.state["final_state"], "interrupted");
        assert_eq!(checkpoint.state["interruption_kind"], "budget_exhausted");
    }

    #[tokio::test]
    async fn finalize_headless_task_result_persists_checkpoint_without_output_file() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let tid = create_in_progress_task(&svc).await;

        let mut sr = stream_result_for_task_checkpoint();
        sr.session_persistence_error = Some("write task output: permission denied".into());

        let exit_code =
            finalize_headless_task_result(&svc, &tid, &sr, Some("fallback-session"), None)
                .await
                .unwrap();

        assert_eq!(exit_code, ExitCode::PersistenceError);

        let record = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(record.status, astra_services::TaskStatus::Failed);
        let checkpoint = record.checkpoint.expect("checkpoint should be saved");
        assert!(checkpoint.state.get("output_file").is_none());
        assert_eq!(
            checkpoint.state["persistence_error"],
            "write task output: permission denied"
        );
    }
}
