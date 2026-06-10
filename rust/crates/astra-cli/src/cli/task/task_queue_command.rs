use crate::cli::arg_render::join_words;
use crate::cli::cli_config::cli_args::JobQueueArgs;
use crate::cli::cli_config::cli_context::CliContext;
use crate::cli::cli_config::cli_utils::{cli_user_id, prefix_chars};
use crate::cli::exit_code::ExitCode;
use crate::cli::session::session_runtime;
use crate::cli::task::task_command_utils::task_run_title;
use crate::cli::theme;
use crossterm::style::Stylize;

pub(crate) async fn execute_job_queue(
    args: JobQueueArgs,
    cli_context: &CliContext,
) -> Result<ExitCode, String> {
    let prompt = join_words(&args.text);
    if prompt.trim().is_empty() {
        return Err("job prompt cannot be empty".to_string());
    }

    // This CLI subcommand has no profile in scope. Token resolution is env-only;
    // per-user sessions should use `astra job worker`, which threads profile.
    let (svc, _) = session_runtime::resolve_cloud_task_runtime(None).await?;
    let session_id = cli_context
        .session_id
        .clone()
        .unwrap_or_else(|| "cloud-queue".into());
    let user_id = cli_user_id();
    let task_id = svc
        .create_task(
            &user_id,
            &session_id,
            astra_services::TaskCreateRequest {
                title: task_run_title(&prompt),
                description: Some(prompt.clone()),
                plan: None,
                parent_task_id: None,
                project_type: Some("cloud-agent".to_string()),
                goal_pattern: None,
            },
        )
        .await?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": task_id,
                "status": "pending",
                "backend": "cloud-api",
            }))
            .unwrap_or_default()
        );
    } else {
        eprintln!(
            "  {} Cloud job queued: {} ({})",
            theme::icon_ok(),
            prompt.chars().take(50).collect::<String>(),
            prefix_chars(&task_id, 8).dim()
        );
        eprintln!(
            "  {}",
            "Run `astra job worker --once` from a cloud agent/worker to claim it.".dim()
        );
    }
    Ok(ExitCode::Success)
}
