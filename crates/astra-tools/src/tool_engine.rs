use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing;

use crate::ToolResult;
use crate::schemas::schema_exists_for_tool;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolEngineRegistrationError {
    #[error("tool handler name cannot be empty")]
    EmptyName,
    #[error("tool handler prefix cannot be empty")]
    EmptyPrefix,
    #[error("tool handler already registered: {name}")]
    DuplicateName { name: String },
    #[error("tool handler prefix already registered: {prefix}")]
    DuplicatePrefix { prefix: String },
}

/// Trusted invocation identity carried beside model/provider-authored tool
/// arguments.
///
/// Tool handlers that need correlation metadata can opt into
/// [`ToolHandler::execute_invocation`]. Keeping this envelope separate from
/// `args` prevents internal runtime identity from violating strict provider
/// schemas or changing the semantic identity of a tool call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolInvocationAdmissionSource {
    #[default]
    Policy,
    ImplicitPolicy,
    ParentApproval,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolInvocationMetadata<'a> {
    pub run_id: Option<&'a str>,
    pub turn_chain_id: Option<&'a str>,
    pub tool_call_id: Option<&'a str>,
    pub admission_source: Option<ToolInvocationAdmissionSource>,
    /// Durable user-intent epoch used by the invocation's atomic action
    /// admission. Provider arguments cannot populate this field.
    pub expected_control_epoch: Option<i64>,
}

#[async_trait]
pub trait ToolHandler<C: Sync>: Send + Sync {
    async fn execute(
        &self,
        context: &C,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> ToolResult;

    async fn execute_invocation(
        &self,
        context: &C,
        args: &Value,
        _invocation: ToolInvocationMetadata<'_>,
        cancel_token: Option<&CancellationToken>,
    ) -> ToolResult {
        self.execute(context, args, cancel_token).await
    }
}

#[async_trait]
pub trait DynamicToolHandler<C: Sync>: Send + Sync {
    async fn execute(
        &self,
        name: &str,
        context: &C,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> ToolResult;

    async fn execute_invocation(
        &self,
        name: &str,
        context: &C,
        args: &Value,
        _invocation: ToolInvocationMetadata<'_>,
        cancel_token: Option<&CancellationToken>,
    ) -> ToolResult {
        self.execute(name, context, args, cancel_token).await
    }
}

#[derive(Clone)]
struct PrefixHandler<C: Sync> {
    prefix: String,
    name_validator: Option<fn(&str) -> bool>,
    handler: Arc<dyn DynamicToolHandler<C>>,
}

impl<C: Sync> PrefixHandler<C> {
    fn matches(&self, name: &str) -> bool {
        name.starts_with(&self.prefix)
            && self.name_validator.is_none_or(|validator| validator(name))
    }
}

#[derive(Clone)]
pub struct ToolEngine<C: Sync> {
    handlers: HashMap<String, Arc<dyn ToolHandler<C>>>,
    prefix_handlers: Vec<PrefixHandler<C>>,
}

impl<C: Sync> Default for ToolEngine<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Sync> ToolEngine<C> {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            prefix_handlers: Vec::new(),
        }
    }

    pub fn register_handler<H>(
        &mut self,
        name: impl Into<String>,
        handler: H,
    ) -> Result<(), ToolEngineRegistrationError>
    where
        H: ToolHandler<C> + 'static,
    {
        let name = name.into();
        let normalized = name.trim();
        if normalized.is_empty() {
            return Err(ToolEngineRegistrationError::EmptyName);
        }
        if self.handlers.contains_key(normalized) {
            return Err(ToolEngineRegistrationError::DuplicateName {
                name: normalized.to_string(),
            });
        }

        // Validate that the handler's schema exists in the tool registry.
        // A handler registered without a corresponding schema means the LLM
        // will never call it (it's not in the advertised tool list), or worse,
        // the schema describes args/behavior the handler doesn't implement.
        // Catch this mismatch at registration time rather than runtime.
        if !schema_exists_for_tool(normalized) {
            tracing::warn!(
                tool_name = %normalized,
                "ToolHandler registered without a matching schema in all_tool_schemas(); \
                 the tool will not be advertised to the LLM and may indicate a \
                 schema↔handler mismatch"
            );
        }

        self.handlers
            .insert(normalized.to_string(), Arc::new(handler));
        Ok(())
    }

    pub fn register_prefix_handler<H>(
        &mut self,
        prefix: impl Into<String>,
        handler: H,
    ) -> Result<(), ToolEngineRegistrationError>
    where
        H: DynamicToolHandler<C> + 'static,
    {
        self.register_prefix_handler_impl(prefix, None, handler)
    }

    pub fn register_prefix_handler_with_validator<H>(
        &mut self,
        prefix: impl Into<String>,
        name_validator: fn(&str) -> bool,
        handler: H,
    ) -> Result<(), ToolEngineRegistrationError>
    where
        H: DynamicToolHandler<C> + 'static,
    {
        self.register_prefix_handler_impl(prefix, Some(name_validator), handler)
    }

    fn register_prefix_handler_impl<H>(
        &mut self,
        prefix: impl Into<String>,
        name_validator: Option<fn(&str) -> bool>,
        handler: H,
    ) -> Result<(), ToolEngineRegistrationError>
    where
        H: DynamicToolHandler<C> + 'static,
    {
        let prefix = prefix.into();
        let normalized = prefix.trim();
        if normalized.is_empty() {
            return Err(ToolEngineRegistrationError::EmptyPrefix);
        }
        if self
            .prefix_handlers
            .iter()
            .any(|entry| entry.prefix == normalized)
        {
            return Err(ToolEngineRegistrationError::DuplicatePrefix {
                prefix: normalized.to_string(),
            });
        }
        self.prefix_handlers.push(PrefixHandler {
            prefix: normalized.to_string(),
            name_validator,
            handler: Arc::new(handler),
        });
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
            || self.prefix_handlers.iter().any(|entry| entry.matches(name))
    }

    pub fn handler_names(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(String::as_str)
    }

    pub async fn execute(
        &self,
        name: &str,
        context: &C,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Option<ToolResult> {
        self.execute_invocation(
            name,
            context,
            args,
            ToolInvocationMetadata::default(),
            cancel_token,
        )
        .await
    }

    pub async fn execute_invocation(
        &self,
        name: &str,
        context: &C,
        args: &Value,
        invocation: ToolInvocationMetadata<'_>,
        cancel_token: Option<&CancellationToken>,
    ) -> Option<ToolResult> {
        if let Some(handler) = self.handlers.get(name) {
            return Some(
                AssertUnwindSafe(handler.execute_invocation(
                    context,
                    args,
                    invocation,
                    cancel_token,
                ))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    tracing::error!(
                        tool_name = %name,
                        "tool handler panicked; returning error to caller"
                    );
                    ToolResult::error(format!("Internal error: tool '{name}' panicked"))
                }),
            );
        }
        let handler = self
            .prefix_handlers
            .iter()
            .find(|entry| entry.matches(name))?;
        Some(
            AssertUnwindSafe(handler.handler.execute_invocation(
                name,
                context,
                args,
                invocation,
                cancel_token,
            ))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                tracing::error!(
                    tool_name = %name,
                    "prefix handler panicked; returning error to caller"
                );
                ToolResult::error(format!("Internal error: tool '{name}' panicked"))
            }),
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NotifyToolHandler;

#[async_trait]
impl<C> ToolHandler<C> for NotifyToolHandler
where
    C: Send + Sync,
{
    async fn execute(
        &self,
        _context: &C,
        args: &Value,
        _cancel_token: Option<&CancellationToken>,
    ) -> ToolResult {
        let message = string_arg(args, "message").unwrap_or("");
        if message.is_empty() {
            ToolResult::error("Error: notify requires a non-empty message".to_string())
        } else {
            ToolResult::text(format!("Notification: {message}"))
        }
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug)]
    struct TestContext {
        prefix: &'static str,
    }

    #[derive(Debug)]
    struct EchoHandler;

    #[async_trait]
    impl ToolHandler<TestContext> for EchoHandler {
        async fn execute(
            &self,
            context: &TestContext,
            args: &Value,
            _cancel_token: Option<&CancellationToken>,
        ) -> ToolResult {
            let value = string_arg(args, "value").unwrap_or("");
            ToolResult::text(format!("{}:{value}", context.prefix))
        }
    }

    #[derive(Debug)]
    struct DynamicEchoHandler;

    #[async_trait]
    impl DynamicToolHandler<TestContext> for DynamicEchoHandler {
        async fn execute(
            &self,
            name: &str,
            context: &TestContext,
            args: &Value,
            _cancel_token: Option<&CancellationToken>,
        ) -> ToolResult {
            let value = string_arg(args, "value").unwrap_or("");
            ToolResult::text(format!("{}:{name}:{value}", context.prefix))
        }
    }

    #[derive(Debug)]
    struct InvocationAwareHandler;

    #[async_trait]
    impl ToolHandler<TestContext> for InvocationAwareHandler {
        async fn execute(
            &self,
            _context: &TestContext,
            _args: &Value,
            _cancel_token: Option<&CancellationToken>,
        ) -> ToolResult {
            ToolResult::error("invocation envelope missing".to_string())
        }

        async fn execute_invocation(
            &self,
            _context: &TestContext,
            args: &Value,
            invocation: ToolInvocationMetadata<'_>,
            _cancel_token: Option<&CancellationToken>,
        ) -> ToolResult {
            ToolResult::text(
                json!({
                    "arguments": args,
                    "run_id": invocation.run_id,
                    "turn_chain_id": invocation.turn_chain_id,
                    "tool_call_id": invocation.tool_call_id,
                    "admission_source": format!("{:?}", invocation.admission_source),
                })
                .to_string(),
            )
        }
    }

    #[tokio::test]
    async fn registered_handler_executes_with_context() {
        let mut engine = ToolEngine::new();
        engine.register_handler("echo", EchoHandler).unwrap();

        let result = engine
            .execute(
                "echo",
                &TestContext { prefix: "ctx" },
                &json!({"value": "ok"}),
                None,
            )
            .await
            .expect("registered handler should execute");

        assert_eq!(result.output, "ctx:ok");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn invocation_metadata_reaches_handler_without_mutating_arguments() {
        let mut engine = ToolEngine::new();
        engine
            .register_handler("invocation_aware", InvocationAwareHandler)
            .unwrap();
        let args = json!({"query": "status", "_provider_cursor": "cursor-7"});

        let result = engine
            .execute_invocation(
                "invocation_aware",
                &TestContext { prefix: "ctx" },
                &args,
                ToolInvocationMetadata {
                    run_id: Some("run-1"),
                    turn_chain_id: Some("turn-1"),
                    tool_call_id: Some("call-1"),
                    admission_source: Some(ToolInvocationAdmissionSource::Policy),
                    expected_control_epoch: None,
                },
                None,
            )
            .await
            .expect("registered handler should execute");
        let output: Value = serde_json::from_str(&result.output).unwrap();

        assert_eq!(output["arguments"], args);
        assert_eq!(output["run_id"], "run-1");
        assert_eq!(output["turn_chain_id"], "turn-1");
        assert_eq!(output["tool_call_id"], "call-1");
        assert_eq!(output["admission_source"], "Some(Policy)");
        assert!(output["arguments"].get("_run_id").is_none());
        assert!(output["arguments"].get("_turn_chain_id").is_none());
        assert!(output["arguments"].get("_tool_call_id").is_none());
    }

    #[tokio::test]
    async fn unknown_handler_returns_none() {
        let engine: ToolEngine<TestContext> = ToolEngine::new();

        assert!(
            engine
                .execute("missing", &TestContext { prefix: "ctx" }, &json!({}), None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn prefix_handler_executes_dynamic_tool_names() {
        let mut engine = ToolEngine::new();
        engine
            .register_prefix_handler("mcp__", DynamicEchoHandler)
            .unwrap();

        assert!(engine.contains("mcp__demo__search"));
        assert!(!engine.contains("demo__search"));

        let result = engine
            .execute(
                "mcp__demo__search",
                &TestContext { prefix: "ctx" },
                &json!({"value": "ok"}),
                None,
            )
            .await
            .expect("prefix handler should execute");

        assert_eq!(result.output, "ctx:mcp__demo__search:ok");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn validated_prefix_handler_rejects_invalid_dynamic_tool_names() {
        let mut engine = ToolEngine::new();
        engine
            .register_prefix_handler_with_validator(
                "mcp__",
                astra_core::tool_offer::is_mcp_namespaced_tool_name,
                DynamicEchoHandler,
            )
            .unwrap();

        assert!(engine.contains("mcp__demo__search"));
        assert!(!engine.contains("mcp__"));
        assert!(!engine.contains("mcp__bad/name"));

        assert!(
            engine
                .execute(
                    "mcp__bad/name",
                    &TestContext { prefix: "ctx" },
                    &json!({"value": "bad"}),
                    None,
                )
                .await
                .is_none()
        );
    }

    #[test]
    fn duplicate_and_empty_handler_names_are_rejected() {
        let mut engine = ToolEngine::<TestContext>::new();

        assert_eq!(
            engine.register_handler(" ", EchoHandler),
            Err(ToolEngineRegistrationError::EmptyName)
        );
        engine.register_handler("echo", EchoHandler).unwrap();
        assert_eq!(
            engine.register_handler(" echo ", EchoHandler),
            Err(ToolEngineRegistrationError::DuplicateName {
                name: "echo".to_string(),
            })
        );
        assert_eq!(
            engine.register_prefix_handler(" ", DynamicEchoHandler),
            Err(ToolEngineRegistrationError::EmptyPrefix)
        );
        engine
            .register_prefix_handler("mcp__", DynamicEchoHandler)
            .unwrap();
        assert_eq!(
            engine.register_prefix_handler(" mcp__ ", DynamicEchoHandler),
            Err(ToolEngineRegistrationError::DuplicatePrefix {
                prefix: "mcp__".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn notify_handler_requires_non_empty_message() {
        let mut engine = ToolEngine::new();
        engine
            .register_handler("notify", NotifyToolHandler)
            .expect("notify handler registration should succeed");

        let ok = engine
            .execute(
                "notify",
                &TestContext { prefix: "unused" },
                &json!({"message": " hello "}),
                None,
            )
            .await
            .expect("notify handler should execute");
        assert_eq!(ok.output, "Notification: hello");
        assert!(!ok.is_error);

        let err = engine
            .execute(
                "notify",
                &TestContext { prefix: "unused" },
                &json!({"message": "   "}),
                None,
            )
            .await
            .expect("notify handler should execute");
        assert!(err.is_error);
        assert_eq!(err.output, "Error: notify requires a non-empty message");
    }

    /// Regression: when a tool handler panics, the engine must catch the
    /// unwind and return a ToolResult::error() instead of propagating the
    /// panic to the async runtime.
    #[tokio::test]
    async fn panicking_handler_is_caught_and_converted_to_error() {
        #[derive(Debug)]
        struct PanicHandler;

        #[async_trait]
        impl ToolHandler<TestContext> for PanicHandler {
            async fn execute(
                &self,
                _context: &TestContext,
                _args: &Value,
                _cancel_token: Option<&CancellationToken>,
            ) -> ToolResult {
                panic!("handler exploded intentionally");
            }
        }

        let mut engine = ToolEngine::new();
        engine.register_handler("panic_bomb", PanicHandler).unwrap();

        // Must not propagate the panic — must return an error ToolResult.
        let result = engine
            .execute(
                "panic_bomb",
                &TestContext { prefix: "ctx" },
                &json!({}),
                None,
            )
            .await;

        let tool_result = result.expect("engine must return Some even on panic");
        assert!(tool_result.is_error, "panic must yield an error result");
        assert!(
            engine
                .execute("missing", &TestContext { prefix: "ctx" }, &json!({}), None)
                .await
                .is_none()
        );
    }

    /// Regression: panicking prefix handler must also be caught.
    #[tokio::test]
    async fn panicking_prefix_handler_is_caught() {
        #[derive(Debug)]
        struct PanicPrefixHandler;

        #[async_trait]
        impl DynamicToolHandler<TestContext> for PanicPrefixHandler {
            async fn execute(
                &self,
                _name: &str,
                _context: &TestContext,
                _args: &Value,
                _cancel_token: Option<&CancellationToken>,
            ) -> ToolResult {
                panic!("prefix handler exploded");
            }
        }

        let mut engine = ToolEngine::new();
        engine
            .register_prefix_handler("explode__", PanicPrefixHandler)
            .unwrap();

        let result = engine
            .execute(
                "explode__test",
                &TestContext { prefix: "ctx" },
                &json!({}),
                None,
            )
            .await;

        let tool_result = result.expect("engine must return Some even on panic");
        assert!(tool_result.is_error);
        assert!(tool_result.output.contains("panicked"));
    }
}
