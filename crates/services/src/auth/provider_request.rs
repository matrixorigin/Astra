use astra_core::{ErrorResponse, ProviderCapabilityDescriptorConfig, error_response_coded};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

use crate::runs::RuntimeCapabilityDescriptorRequest;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRequestDescriptor {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderAuthorizedRequest {
    pub provider_id: String,
    pub external_subject: String,
    pub provider_scope_id: String,
    pub request_authorization_id: String,
    pub expires_at_unix: i64,
}

pub fn validate_runtime_capability_descriptor(
    descriptor: &RuntimeCapabilityDescriptorRequest,
    expected_type: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_descriptor_strings(
        &descriptor.id,
        &descriptor.descriptor_type,
        &descriptor.transport,
        &descriptor.endpoint_url,
        &descriptor.protocol,
        expected_type,
    )
}

pub fn validate_runtime_capability_descriptor_allowed(
    descriptor: &RuntimeCapabilityDescriptorRequest,
    expected_type: &str,
    allowed: &[ProviderCapabilityDescriptorConfig],
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_runtime_capability_descriptor(descriptor, expected_type)?;
    if allowed.iter().any(|candidate| {
        candidate.id == descriptor.id
            && candidate.descriptor_type == descriptor.descriptor_type
            && candidate.transport == descriptor.transport
            && candidate.endpoint_url == descriptor.endpoint_url
            && candidate.protocol == descriptor.protocol
    }) {
        return Ok(());
    }
    Err(provider_context_invalid(format!(
        "capability descriptor {}:{} is not allowed for this provider",
        descriptor.descriptor_type, descriptor.id
    )))
}

fn validate_descriptor_strings(
    id: &str,
    descriptor_type: &str,
    transport: &str,
    endpoint_url: &str,
    protocol: &str,
    expected_type: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_exact_descriptor_string("capability_descriptors[].id", id)?;
    validate_exact_descriptor_string("capability_descriptors[].type", descriptor_type)?;
    validate_exact_descriptor_string("capability_descriptors[].transport", transport)?;
    validate_exact_descriptor_string("capability_descriptors[].protocol", protocol)?;
    if descriptor_type != expected_type {
        return Err(provider_context_invalid(format!(
            "capability descriptor type must be {expected_type}"
        )));
    }
    validate_endpoint_url(endpoint_url)
}

fn validate_exact_descriptor_string(
    field: &str,
    value: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(provider_context_invalid(format!(
            "{field} must be a non-empty exact string"
        )));
    }
    Ok(())
}

fn validate_endpoint_url(url: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_exact_descriptor_string("capability_descriptors[].endpoint_url", url)?;
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        provider_context_invalid(format!(
            "capability descriptor endpoint_url is invalid: {error}"
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(provider_context_invalid(
            "capability descriptor endpoint_url must be an absolute http or https URL",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(provider_context_invalid(
            "capability descriptor endpoint_url must not contain userinfo or fragment",
        ));
    }
    Ok(())
}

fn provider_context_invalid(detail: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::BAD_REQUEST,
        detail,
        "provider_runtime_context_invalid",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn descriptor(descriptor_type: &str, endpoint_url: &str) -> RuntimeCapabilityDescriptorRequest {
        RuntimeCapabilityDescriptorRequest {
            id: "moi-model-gateway".to_string(),
            descriptor_type: descriptor_type.to_string(),
            transport: "http".to_string(),
            endpoint_url: endpoint_url.to_string(),
            protocol: "moi-runtime-model-gateway.v1".to_string(),
            metadata: Map::new(),
        }
    }

    fn allowance(descriptor_type: &str, endpoint_url: &str) -> ProviderCapabilityDescriptorConfig {
        ProviderCapabilityDescriptorConfig {
            id: "moi-model-gateway".to_string(),
            descriptor_type: descriptor_type.to_string(),
            transport: "http".to_string(),
            endpoint_url: endpoint_url.to_string(),
            protocol: "moi-runtime-model-gateway.v1".to_string(),
        }
    }

    #[test]
    fn validate_runtime_capability_descriptor_accepts_expected_type() {
        validate_runtime_capability_descriptor(
            &descriptor("model_gateway", "http://127.0.0.1/model"),
            "model_gateway",
        )
        .expect("descriptor should be accepted");
    }

    #[test]
    fn validate_runtime_capability_descriptor_rejects_wrong_type() {
        let (status, body) = validate_runtime_capability_descriptor(
            &descriptor("mcp", "http://127.0.0.1/model"),
            "model_gateway",
        )
        .expect_err("wrong type should be rejected");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0.error_code.as_deref(),
            Some("provider_runtime_context_invalid")
        );
    }

    #[test]
    fn validate_runtime_capability_descriptor_allowed_requires_exact_provider_allowlist_match() {
        validate_runtime_capability_descriptor_allowed(
            &descriptor("model_gateway", "http://127.0.0.1/model"),
            "model_gateway",
            &[allowance("model_gateway", "http://127.0.0.1/model")],
        )
        .expect("exactly allowed descriptor should pass");

        let (status, body) = validate_runtime_capability_descriptor_allowed(
            &descriptor("model_gateway", "http://127.0.0.1/other"),
            "model_gateway",
            &[allowance("model_gateway", "http://127.0.0.1/model")],
        )
        .expect_err("different endpoint path must not be accepted");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0.error_code.as_deref(),
            Some("provider_runtime_context_invalid")
        );
        assert!(body.0.detail.contains("not allowed"));
    }
}
