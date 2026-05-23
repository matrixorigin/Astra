//! Mock MCP server for integration testing.
//!
//! Exposes three tools: echo, add, get_time.
//! Uses stdio transport — spawn as a subprocess from integration tests.
//!
//! Build: `cargo build --example mock_mcp_server`
//! Binary: `target/debug/examples/mock_mcp_server`

use rmcp::handler::server::router::Router;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::io::stdio;
use rmcp::{serve_server, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Clone)]
struct MockMcpServer;

#[derive(Deserialize, JsonSchema)]
struct EchoParams {
    /// The message to echo back
    message: String,
}

#[derive(Deserialize, JsonSchema)]
struct AddParams {
    /// First integer
    a: i64,
    /// Second integer
    b: i64,
}

#[tool_router(server_handler)]
impl MockMcpServer {
    #[tool(description = "Echo back the input message")]
    async fn echo(&self, Parameters(params): Parameters<EchoParams>) -> String {
        params.message
    }

    #[tool(description = "Add two integers together")]
    async fn add(&self, Parameters(params): Parameters<AddParams>) -> String {
        (params.a + params.b).to_string()
    }

    #[tool(description = "Get the current server time in RFC 3339 format")]
    async fn get_time(&self) -> String {
        chrono::Utc::now().to_rfc3339()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Router::new takes the service, then register tools from the generated ToolRouter.
    // ToolRouter<S> implements IntoIterator<Item = ToolRoute<S>>, so we pass it to with_tools.
    let tool_router = MockMcpServer::tool_router();
    let router = Router::new(MockMcpServer).with_tools(tool_router);
    let service = serve_server(router, stdio()).await?;
    service.waiting().await?;
    Ok(())
}
