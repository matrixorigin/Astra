use std::net::SocketAddr;

use astra_core::config::AppSettings;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
