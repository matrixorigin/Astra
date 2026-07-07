use std::net::SocketAddr;

use astra_core::config::AppSettings;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = AppSettings::from_env()?;
    let addr: SocketAddr = format!("{}:{}", settings.api.host, settings.api.port).parse()?;

    astra_runtime::serve(addr).await?;
    Ok(())
}
