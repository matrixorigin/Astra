use astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext;
use astra_tools::ToolResult;
use serde_json::Value;

const MOI_RUNTIME_AUTHORIZATION_ENV: &str = "MOI_RUNTIME_AUTHORIZATION";

pub(crate) async fn execute_bash(
    executor: &astra_tools::executor::DefaultToolExecutor,
    args: &Value,
    context: &RuntimeProcessAuthorizationContext,
) -> ToolResult {
    let environment = vec![(
        MOI_RUNTIME_AUTHORIZATION_ENV.to_string(),
        context.authorization.clone(),
    )];
    astra_tools::shell_ops::execute_bash_with_environment(executor.context(), args, &environment)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn managed_edge_bash_receives_call_scoped_runtime_authorization() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join(".moi");
        std::fs::create_dir_all(&workspace).unwrap();
        let context = RuntimeProcessAuthorizationContext {
            authorization: "Bearer task-scoped-grant".to_string(),
        };
        let executor = astra_tools::executor::DefaultToolExecutor::for_workspace(
            &workspace,
            "user-1",
            "session-1",
            "astra-edge/test",
            Duration::from_secs(30),
        );

        let result = execute_bash(
            &executor,
            &json!({"command": "printf %s \"$MOI_RUNTIME_AUTHORIZATION\""}),
            &context,
        )
        .await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "Bearer task-scoped-grant");
    }
}
