//! Persistable identity of admitted model behavior, never execution authority.

use super::{
    AdmittedModelExecution, ModelAccessKind, ModelExecutionPlacement, PromptCacheCapabilityData,
    ThinkingCapability, ThinkingProtocol,
};
use crate::auth::FernetTokenEncryptor;
use astra_core::canonical_json_string;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Secret-free model facts plus a keyed identity of private routing/overrides.
/// This is only the model portion of an execution freeze, not a complete
/// Evaluation configuration or a replacement for current authorization.
/// Digest-key rotation intentionally invalidates equality and requires reprepare.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelExecutionProjection {
    pub schema_version: u32,
    pub offering_id: String,
    pub access_kind: ModelAccessKind,
    pub execution_placement: ModelExecutionPlacement,
    pub model_name: String,
    pub wire_model_name: Option<String>,
    pub provider: String,
    pub cache_capability: Option<PromptCacheCapabilityData>,
    pub thinking_capability: Option<ThinkingCapability>,
    pub fixed_temperature: Option<f64>,
    pub thinking_protocol: Option<ThinkingProtocol>,
    pub context_window: Option<u32>,
    pub max_completion_tokens: Option<u32>,
    pub request_timeout_ms: Option<u64>,
    pub private_route_and_overrides_digest: String,
}

impl AdmittedModelExecution {
    /// Project this exact admitted value without resolving or reading ambient
    /// configuration. Authentication is excluded, not hashed. All other private
    /// route/override values participate in a domain-separated keyed digest.
    /// Unknown credential conventions remain opaque digest inputs; callers must
    /// use recognized authentication fields to support credential-only rotation.
    pub fn freeze_projection(
        &self,
        encryptor: &FernetTokenEncryptor,
    ) -> Result<ModelExecutionProjection, String> {
        if self
            .fixed_temperature
            .is_some_and(|value| !value.is_finite())
        {
            return Err("model execution fixed temperature must be finite".into());
        }
        let mut headers = BTreeMap::new();
        for (name, value) in &self.header_overrides {
            if authentication_field(&name.to_ascii_lowercase()) {
                continue;
            }
            if headers.insert(name.to_ascii_lowercase(), value).is_some() {
                return Err("model execution contains duplicate case-insensitive headers".into());
            }
        }
        let private = json!({
            "schema_version": 1,
            "base_url": route_identity(&self.base_url)?,
            "completions_url_override": self.completions_url_override.as_deref()
                .map(route_identity).transpose()?,
            "header_overrides": headers,
            "request_body_overrides": self.request_body_overrides.as_ref()
                .and_then(without_authentication),
        });
        Ok(ModelExecutionProjection {
            schema_version: 1,
            offering_id: self.offering_id.clone(),
            access_kind: self.access_kind,
            execution_placement: self.execution_placement,
            model_name: self.model_name.clone(),
            wire_model_name: self.wire_model_name.clone(),
            provider: self.provider.clone(),
            cache_capability: self.cache_capability,
            thinking_capability: self.thinking_capability,
            fixed_temperature: self.fixed_temperature,
            thinking_protocol: self.thinking_protocol,
            context_window: self.context_window,
            max_completion_tokens: self.max_completion_tokens,
            request_timeout_ms: self.request_timeout_ms,
            private_route_and_overrides_digest: encryptor.keyed_digest(
                "astra:model-execution-projection:private-route-and-overrides:v1",
                &canonical_json_string(&private),
            ),
        })
    }
}

// Exact credential names only. Generic `token`, `key`, `secret`, `signature`,
// etc. may describe model behavior and must remain fingerprinted.
fn authentication_field(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-auth-token"
            | "x-access-token"
            | "access_token"
            | "refresh_token"
            | "api_key"
            | "api-key"
            | "x-api-key"
            | "client_secret"
            | "client_assertion"
    )
}

fn without_authentication(object: &serde_json::Map<String, Value>) -> Option<Value> {
    // Only canonical top-level credential fields are transport authentication.
    // Nested objects (including tool schemas and provider-specific parameters)
    // are model semantics, even when their keys resemble credentials.
    let filtered: serde_json::Map<String, Value> = object
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "api_key" | "access_token" | "authorization" | "client_secret" | "client_assertion"
            )
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    // Absent, empty, and credential-only overrides all add no model behavior.
    (!filtered.is_empty()).then_some(Value::Object(filtered))
}

fn route_identity(raw: &str) -> Result<Value, String> {
    // Endpoint admission intentionally leaves base_url empty.
    if raw.is_empty() {
        return Ok(Value::Null);
    }
    let mut url = reqwest::Url::parse(raw)
        .map_err(|_| "model execution route must be an absolute HTTP(S) URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("model execution route must be an absolute HTTP(S) URL".into());
    }
    url.set_username("")
        .map_err(|_| "model execution route cannot remove authentication")?;
    url.set_password(None)
        .map_err(|_| "model execution route cannot remove authentication")?;
    // Query names are case-sensitive: API_KEY is not the credential api_key.
    // Keep query sequence and duplicates: provider routing may depend on them.
    let query: Vec<_> = url
        .query_pairs()
        .filter(|(name, _)| !authentication_field(name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    // Fragments are not sent in HTTP requests.
    url.set_fragment(None);
    Ok(json!({ "url": url.as_str(), "query": query }))
}
