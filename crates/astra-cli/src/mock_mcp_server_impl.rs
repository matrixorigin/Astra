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

#[derive(Deserialize, JsonSchema)]
struct ApplyThenDropAckParams {
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct WriteMarkerParams {
    path: String,
    message: String,
}

#[tool_router(server_handler)]
impl MockMcpServer {
    #[tool(
        description = "Echo back the input message",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn echo(&self, Parameters(params): Parameters<EchoParams>) -> String {
        params.message
    }

    #[tool(
        description = "Add two integers together",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn add(&self, Parameters(params): Parameters<AddParams>) -> String {
        (params.a + params.b).to_string()
    }

    #[tool(
        description = "Get the current server time in RFC 3339 format",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn get_time(&self) -> String {
        chrono::Utc::now().to_rfc3339()
    }

    #[tool(
        description = "Return an acknowledged invalid-parameters JSON-RPC error",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn reject_parameters(&self) -> Result<String, rmcp::ErrorData> {
        Err(rmcp::ErrorData::invalid_params(
            "fixture parameter rejection",
            Some(serde_json::json!({"field": "message"})),
        ))
    }

    #[tool(
        description = "Return an acknowledged tool failure",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn tool_failure(&self) -> rmcp::model::CallToolResult {
        rmcp::model::CallToolResult::error(vec![rmcp::model::Content::text("fixture tool failure")])
    }

    /// Test fixture for a remote mutation whose acknowledgement is lost.
    #[tool(
        description = "Append an applied marker and close before acknowledging",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn apply_then_drop_ack(
        &self,
        Parameters(params): Parameters<ApplyThenDropAckParams>,
    ) -> String {
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&params.path)
            .expect("open MCP fixture counter");
        file.write_all(b"applied\n")
            .expect("write MCP fixture counter");
        file.sync_all().expect("sync MCP fixture counter");
        std::process::exit(0);
    }

    #[tool(
        description = "Write a marker and explicitly settle the workspace effect",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn write_marker(
        &self,
        Parameters(params): Parameters<WriteMarkerParams>,
    ) -> rmcp::model::CallToolResult {
        use std::io::Write;

        let result = std::fs::File::create(&params.path)
            .and_then(|mut file| file.write_all(params.message.as_bytes()))
            .map(|_| "written".to_string())
            .unwrap_or_else(|error| format!("write failed: {error}"));
        let mut response =
            rmcp::model::CallToolResult::success(vec![rmcp::model::Content::text(result)]);
        response.meta = Some(rmcp::model::Meta(serde_json::Map::from_iter([(
            "astra.workspace_effect".to_string(),
            serde_json::json!({"settled": true}),
        )])));
        response
    }
}

pub(crate) async fn run_mock_mcp_server() -> Result<(), Box<dyn std::error::Error>> {
    // Deterministically fail a restart after the mutation fixture has applied.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--exit-if-file-exists") {
        let path = args.next().ok_or("missing fixture marker path")?;
        if std::path::Path::new(&path).exists() {
            return Ok(());
        }
    }
    let tool_router = MockMcpServer::tool_router();
    let router = Router::new(MockMcpServer).with_tools(tool_router);
    let service = serve_server(router, stdio()).await?;
    service.waiting().await?;
    Ok(())
}
