fn main() -> Result<(), Box<dyn std::error::Error>> {
    let process_capture =
        astra_core::history_work_baseline::ProductionProcessCaptureGuard::from_env(
            astra_core::history_work_baseline::ProductionProcessRole::Cli,
        )?;
    let exit_code = astra_cli::entrypoint::run();
    if let Some(process_capture) = process_capture {
        process_capture.finish()?;
    }
    std::process::exit(exit_code);
}
