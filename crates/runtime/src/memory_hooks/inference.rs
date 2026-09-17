//! Typed execution boundary for memory-related inference.
//!
//! Memory consumers depend on [`MemoryInferencePort`], not provider connection
//! details. Server-side callers use [`DirectMemoryInferenceClient`]; edge CLI
//! callers implement the same port through Astra Server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use astra_turn_core::thinking_config::ThinkingConfig;
use astra_turn_types::InferencePurpose;
use async_trait::async_trait;
use serde_json::Value;

use crate::turn::llm::client::{LlmCall, LlmExecutionRoute, global_llm_client};

#[cfg(test)]
use crate::turn::llm::client::call_llm_nonstream;

/// One typed inference request issued by memory extraction or retrieval.
///
/// Purpose is explicit at this boundary so an Astra Server proxy can preserve
/// attribution while a direct provider adapter can keep Astra-only metadata
/// out of the upstream provider payload.
#[derive(Debug, Clone, Copy)]
pub struct MemoryInferenceRequest<'a> {
    pub purpose: InferencePurpose,
    pub invocation_scope: &'a astra_turn_types::InferenceInvocationScope,
    pub messages: &'a [Value],
    pub max_output_tokens: usize,
    pub temperature: f64,
    pub deadline: Duration,
}

/// Execution boundary used by all memory inference consumers.
///
/// Implementations may execute directly against a provider or through an
/// authenticated Astra Server. Consumers never infer transport from a URL or
/// provider name.
#[async_trait]
pub trait MemoryInferencePort: Send + Sync + std::fmt::Debug {
    fn model_name(&self) -> &str;

    async fn complete(
        &self,
        request: MemoryInferenceRequest<'_>,
    ) -> Result<String, astra_core::ClassifiedError>;
}

pub type MemoryInferenceClient = Arc<dyn MemoryInferencePort>;

#[async_trait]
impl<T> MemoryInferencePort for Arc<T>
where
    T: MemoryInferencePort + ?Sized,
{
    fn model_name(&self) -> &str {
        self.as_ref().model_name()
    }

    async fn complete(
        &self,
        request: MemoryInferenceRequest<'_>,
    ) -> Result<String, astra_core::ClassifiedError> {
        self.as_ref().complete(request).await
    }
}

/// Server-only direct-provider implementation of [`MemoryInferencePort`].
#[derive(Clone)]
pub(crate) struct DirectMemoryInferenceClient {
    pub(crate) fixed_temperature: Option<f64>,
    pub(crate) thinking_protocol: Option<astra_core::model_wire::thinking::ThinkingProtocol>,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) model_name: String,
    pub(crate) wire_model_name: Option<String>,
    pub(crate) provider: String,
    pub(crate) header_overrides: HashMap<String, String>,
    pub(crate) request_body_overrides: Option<serde_json::Map<String, serde_json::Value>>,
    pub(crate) completions_url_override: Option<String>,
    pub(crate) request_timeout: Option<Duration>,
}

impl DirectMemoryInferenceClient {
    fn execution_route(&self) -> LlmExecutionRoute<'_> {
        LlmExecutionRoute {
            fixed_temperature: self.fixed_temperature,
            thinking_protocol: self.thinking_protocol,
            model_name: &self.model_name,
            wire_model_name: self.wire_model_name.as_deref(),
            api_key: &self.api_key,
            base_url: &self.base_url,
            provider: &self.provider,
            header_overrides: (!self.header_overrides.is_empty()).then_some(&self.header_overrides),
            request_body_overrides: self.request_body_overrides.as_ref(),
            completions_url_override: self.completions_url_override.as_deref(),
            request_timeout: self.request_timeout,
        }
    }
}

pub(crate) struct DurableMemoryInferenceClient {
    offering_id: String,
    model_name: String,
    shared_pool: astra_core::SharedPool,
    encryptor: Arc<astra_services::FernetTokenEncryptor>,
    user_id: String,
}

impl DurableMemoryInferenceClient {
    pub(crate) fn new(
        offering_id: String,
        model_name: String,
        shared_pool: astra_core::SharedPool,
        encryptor: Arc<astra_services::FernetTokenEncryptor>,
        user_id: impl Into<String>,
    ) -> Self {
        Self {
            offering_id,
            model_name,
            shared_pool,
            encryptor,
            user_id: user_id.into(),
        }
    }
}

impl std::fmt::Debug for DurableMemoryInferenceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableMemoryInferenceClient")
            .field("offering_id", &self.offering_id)
            .field("model_name", &self.model_name)
            .finish()
    }
}

#[async_trait]
impl MemoryInferencePort for DurableMemoryInferenceClient {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    async fn complete(
        &self,
        request: MemoryInferenceRequest<'_>,
    ) -> Result<String, astra_core::ClassifiedError> {
        // Reuse the run/completion admission contract for every background
        // provider attempt. Never retain stale plaintext routes in the client.
        let execution = astra_services::revalidate_admitted_model_execution(
            self.shared_pool.settings(),
            &self.encryptor,
            &self.user_id,
            &self.offering_id,
            Some(self.shared_pool.get()),
        )
        .await
        .map_err(|error| {
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::PolicyDenied,
                format!("Memory model admission failed: {error}"),
            )
        })?;
        let direct = DirectMemoryInferenceClient {
            fixed_temperature: execution.fixed_temperature,
            thinking_protocol: execution.thinking_protocol,
            base_url: execution.base_url.clone(),
            api_key: execution.api_key.clone(),
            model_name: execution.model_name.clone(),
            wire_model_name: execution.wire_model_name.clone(),
            provider: execution.provider.clone(),
            header_overrides: execution.header_overrides.clone(),
            request_body_overrides: execution.request_body_overrides.clone(),
            completions_url_override: None,
            request_timeout: None,
        };
        let ledger = crate::turn::llm::durable::DurableInferenceLedger::new(
            self.shared_pool.clone(),
            &self.user_id,
            execution,
        );
        let result = ledger
            .execute_nonstream(
                global_llm_client(),
                request.invocation_scope.clone(),
                LlmCall {
                    purpose: request.purpose,
                    messages: request.messages,
                    tools: &[],
                    cache_capability: None,
                    route: direct.execution_route(),
                    max_output_tokens: Some(request.max_output_tokens),
                    temperature: Some(request.temperature),
                    has_fallback: false,
                    thinking: &ThinkingConfig::Off,
                },
                request.deadline,
            )
            .await
            .into_result()?;
        Ok(result.full_text)
    }
}

impl std::fmt::Debug for DirectMemoryInferenceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectMemoryInferenceClient")
            .field("model_name", &self.model_name)
            .field("wire_model_name", &self.wire_model_name)
            .field("provider", &self.provider)
            .field("credential_present", &!self.api_key.is_empty())
            .field("header_count", &self.header_overrides.len())
            .field(
                "request_body_overrides_present",
                &self.request_body_overrides.is_some(),
            )
            .field(
                "completions_url_override_present",
                &self.completions_url_override.is_some(),
            )
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

#[cfg(test)]
#[async_trait]
impl MemoryInferencePort for DirectMemoryInferenceClient {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    async fn complete(
        &self,
        request: MemoryInferenceRequest<'_>,
    ) -> Result<String, astra_core::ClassifiedError> {
        let result = call_llm_nonstream(
            global_llm_client(),
            LlmCall {
                purpose: request.purpose,
                messages: request.messages,
                tools: &[],
                cache_capability: None,
                route: self.execution_route(),
                max_output_tokens: Some(request.max_output_tokens),
                temperature: Some(request.temperature),
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            request.deadline,
        )
        .await?;
        Ok(result.full_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_client() -> DirectMemoryInferenceClient {
        DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
            model_name: "qwen-flash".into(),
            wire_model_name: None,
            provider: "openai".into(),
            request_body_overrides: None,
            header_overrides: HashMap::new(),
            completions_url_override: None,
            request_timeout: None,
        }
    }

    #[test]
    fn direct_provider_client_clones_execution_material() {
        let cloned = direct_client().clone();
        assert_eq!(cloned.base_url, "https://api.example.com/v1");
        assert_eq!(cloned.api_key, "sk-test");
        assert_eq!(cloned.model_name, "qwen-flash");
    }

    #[test]
    fn direct_provider_client_debug_redacts_execution_material() {
        let debug = format!("{:?}", direct_client());
        assert!(debug.contains("DirectMemoryInferenceClient"));
        assert!(debug.contains("qwen-flash"));
        assert!(!debug.contains("sk-test"));
        assert!(!debug.contains("api.example.com"));
    }
}
