use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use serde_json::Value;
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

#[async_trait]
pub trait ToolHandler<C>: Send + Sync {
    async fn execute(&self, context: &C, args: &Value) -> ToolResult;
}

#[async_trait]
pub trait DynamicToolHandler<C>: Send + Sync {
    async fn execute(&self, name: &str, context: &C, args: &Value) -> ToolResult;
}

#[derive(Clone)]
struct PrefixHandler<C> {
    prefix: String,
    handler: Arc<dyn DynamicToolHandler<C>>,
}

#[derive(Clone)]
pub struct ToolEngine<C> {
    handlers: HashMap<String, Arc<dyn ToolHandler<C>>>,
    prefix_handlers: Vec<PrefixHandler<C>>,
}

impl<C> Default for ToolEngine<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> ToolEngine<C> {
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
            handler: Arc::new(handler),
        });
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
            || self
                .prefix_handlers
                .iter()
                .any(|entry| name.starts_with(&entry.prefix))
    }

    pub fn handler_names(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(String::as_str)
    }

    pub async fn execute(&self, name: &str, context: &C, args: &Value) -> Option<ToolResult> {
        if let Some(handler) = self.handlers.get(name) {
            return Some(
                AssertUnwindSafe(handler.execute(context, args))
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
            .find(|entry| name.starts_with(&entry.prefix))?;
        Some(
            AssertUnwindSafe(handler.handler.execute(name, context, args))
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
    async fn execute(&self, _context: &C, args: &Value) -> ToolResult {
        let message = string_arg(args, "message").unwrap_or("");
        if message.is_empty() {
            ToolResult::error("Error: notify requires a non-empty message".to_string())
        } else {
            ToolResult::text(format!("Notification: {message}"))
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WebSearchToolHandler;

#[async_trait]
impl<C> ToolHandler<C> for WebSearchToolHandler
where
    C: Send + Sync,
{
    async fn execute(&self, _context: &C, args: &Value) -> ToolResult {
        crate::web_search::web_search_result(args)
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
        async fn execute(&self, context: &TestContext, args: &Value) -> ToolResult {
            let value = string_arg(args, "value").unwrap_or("");
            ToolResult::text(format!("{}:{value}", context.prefix))
        }
    }

    #[derive(Debug)]
    struct DynamicEchoHandler;

    #[async_trait]
    impl DynamicToolHandler<TestContext> for DynamicEchoHandler {
        async fn execute(&self, name: &str, context: &TestContext, args: &Value) -> ToolResult {
            let value = string_arg(args, "value").unwrap_or("");
            ToolResult::text(format!("{}:{name}:{value}", context.prefix))
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
            )
            .await
            .expect("registered handler should execute");

        assert_eq!(result.output, "ctx:ok");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn unknown_handler_returns_none() {
        let engine: ToolEngine<TestContext> = ToolEngine::new();

        assert!(
            engine
                .execute("missing", &TestContext { prefix: "ctx" }, &json!({}))
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
            )
            .await
            .expect("prefix handler should execute");

        assert_eq!(result.output, "ctx:mcp__demo__search:ok");
        assert!(!result.is_error);
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
            )
            .await
            .expect("notify handler should execute");
        assert!(err.is_error);
        assert_eq!(err.output, "Error: notify requires a non-empty message");
    }

    #[tokio::test]
    async fn web_search_handler_uses_structured_error_semantics() {
        let mut engine = ToolEngine::new();
        engine
            .register_handler("web_search", WebSearchToolHandler)
            .expect("web_search handler registration should succeed");

        let ok = engine
            .execute(
                "web_search",
                &TestContext { prefix: "unused" },
                &json!({"query": "astra runtime", "engine": "github"}),
            )
            .await
            .expect("web_search handler should execute");
        assert!(!ok.is_error, "{ok:?}");
        assert!(ok.output.contains("search_url"));

        let err = engine
            .execute(
                "web_search",
                &TestContext { prefix: "unused" },
                &json!({"engine": "github"}),
            )
            .await
            .expect("web_search handler should execute");
        assert!(err.is_error, "{err:?}");
        assert!(err.output.contains("Missing or empty 'query' parameter"));
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
            async fn execute(&self, _context: &TestContext, _args: &Value) -> ToolResult {
                panic!("handler exploded intentionally");
            }
        }

        let mut engine = ToolEngine::new();
        engine.register_handler("panic_bomb", PanicHandler).unwrap();

        // Must not propagate the panic — must return an error ToolResult.
        let result = engine
            .execute("panic_bomb", &TestContext { prefix: "ctx" }, &json!({}))
            .await;

        let tool_result = result.expect("engine must return Some even on panic");
        assert!(tool_result.is_error, "panic must yield an error result");
        assert!(
            tool_result.output.contains("panicked"),
            "error message must mention panic: {}",
            tool_result.output
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
            ) -> ToolResult {
                panic!("prefix handler exploded");
            }
        }

        let mut engine = ToolEngine::new();
        engine
            .register_prefix_handler("explode__", PanicPrefixHandler)
            .unwrap();

        let result = engine
            .execute("explode__test", &TestContext { prefix: "ctx" }, &json!({}))
            .await;

        let tool_result = result.expect("engine must return Some even on panic");
        assert!(tool_result.is_error);
        assert!(tool_result.output.contains("panicked"));
    }
}
