#[path = "../mock_mcp_server_impl.rs"]
mod mock_mcp_server_impl;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    mock_mcp_server_impl::run_mock_mcp_server().await
}
