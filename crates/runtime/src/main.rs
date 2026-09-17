use std::net::SocketAddr;

use astra_core::config::AppSettings;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Keep the private invocation supervisor and artifact identity probe ahead
    // of logging, async runtime, configuration, network, and database setup.
    if let Some(exit_code) = astra_sandbox::run_invocation_supervisor_if_requested() {
        std::process::exit(exit_code);
    }
    if astra_core::build_info::write_json_if_requested()? {
        return Ok(());
    }
    astra_core::process_runtime::build_process_runtime()?.block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let process_capture =
        astra_core::history_work_baseline::ProductionProcessCaptureGuard::from_env(
            astra_core::history_work_baseline::ProductionProcessRole::Server,
        )?;
    let settings = AppSettings::from_env()?;
    let addr: SocketAddr = format!("{}:{}", settings.api.host, settings.api.port).parse()?;

    let serve_result = astra_runtime::serve(addr).await;
    if let Some(process_capture) = process_capture {
        process_capture.finish()?;
    }
    serve_result?;
    Ok(())
}
