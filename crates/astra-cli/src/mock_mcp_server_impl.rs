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
    message: String,
}

#[derive(Deserialize, JsonSchema)]
struct AddParams {
    a: i64,
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

pub(crate) async fn run_mock_mcp_server() -> Result<(), Box<dyn std::error::Error>> {
    let tool_router = MockMcpServer::tool_router();
    let router = Router::new(MockMcpServer).with_tools(tool_router);
    let service = serve_server(router, stdio()).await?;
    service.waiting().await?;
    Ok(())
}
