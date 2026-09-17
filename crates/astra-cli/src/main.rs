fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The invocation supervisor must run before logging, history capture, the
    // Tokio runtime, or any other application threads exist. It is a private,
    // nonce-authenticated helper mode used only by the shell ownership path.
    if let Some(exit_code) = astra_sandbox::run_invocation_supervisor_if_requested() {
        std::process::exit(exit_code);
    }
    if astra_core::build_info::write_json_if_requested()? {
        return Ok(());
    }

    astra_cli::cli::stream::output_sink::configure_process_output_signals()?;

    let process_capture =
        astra_core::history_work_baseline::ProductionProcessCaptureGuard::from_env(
            astra_core::history_work_baseline::ProductionProcessRole::Cli,
        )?;
    let exit_code = astra_cli::entrypoint::run();
    if let Some(process_capture) = process_capture {
        process_capture.finish()?;
    }
    let exit_code = astra_cli::cli::stream::output_sink::resolved_exit_code(exit_code);
    std::process::exit(exit_code);
}
