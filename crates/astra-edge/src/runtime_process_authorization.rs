use astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext;
use astra_tools::ToolResult;
use serde_json::Value;

const MOI_RUNTIME_AUTHORIZATION_ENV: &str = "MOI_RUNTIME_AUTHORIZATION";

pub(crate) struct CommandTraceContext {
    pub traceparent: Option<String>,
    pub tracestate: Option<String>,
}

pub(crate) async fn execute_bash(
    executor: &astra_tools::executor::DefaultToolExecutor,
    args: &Value,
    context: &RuntimeProcessAuthorizationContext,
    trace: &CommandTraceContext,
    cancel: &tokio_util::sync::CancellationToken,
) -> ToolResult {
    let mut environment = vec![(
        MOI_RUNTIME_AUTHORIZATION_ENV.to_string(),
        context.authorization.clone(),
    )];
    let accepted = astra_logging::trace_context::Carrier::from_header_values(
        trace.traceparent.as_deref(),
        trace.tracestate.as_deref(),
    );
    if let Some(traceparent) = accepted.traceparent {
        environment.push(("TRACEPARENT".to_string(), traceparent));
        if let Some(tracestate) = accepted.tracestate {
            environment.push(("TRACESTATE".to_string(), tracestate));
        }
    }
    let mut tool_context = executor.context().clone();
    tool_context.cancel_token = Some(std::sync::Arc::new(cancel.clone()));
    astra_tools::shell_ops::execute_bash_with_environment(&tool_context, args, &environment).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        );

        let result = execute_bash(
            &executor,
            &json!({"command": "printf %s \"$MOI_RUNTIME_AUTHORIZATION\""}),
            &context,
            &CommandTraceContext {
                traceparent: None,
                tracestate: None,
            },
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "Bearer task-scoped-grant");
    }

    #[tokio::test]
    async fn bash_trace_carrier_replaces_an_inherited_value() {
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
        );
        let traceparent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
        unsafe {
            std::env::set_var(
                "TRACEPARENT",
                "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01",
            );
        }
        let result = execute_bash(
            &executor,
            &json!({"command": "printf %s \"$TRACEPARENT\""}),
            &context,
            &CommandTraceContext {
                traceparent: Some(traceparent.to_string()),
                tracestate: None,
            },
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
        unsafe {
            std::env::remove_var("TRACEPARENT");
        }

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, traceparent);
    }
}
