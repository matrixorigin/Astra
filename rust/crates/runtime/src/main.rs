use std::{env, net::SocketAddr};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = env::var("RUST_API_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8001".to_string())
        .parse()?;

    mo_agent_runtime::serve(addr).await?;
    Ok(())
}
