use axum::{Json, http::StatusCode};
use serde::Serialize;
use serde_json::Value;

use astra_core::{ErrorResponse, error_response_coded};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryStatus {
    Active,
    Disabled,
    Invalid,
}

pub fn parse_registry_status(
    entity: &'static str,
    raw: &str,
    code: &'static str,
) -> Result<RegistryStatus, (StatusCode, Json<ErrorResponse>)> {
    match raw {
        "active" => Ok(RegistryStatus::Active),
        "disabled" => Ok(RegistryStatus::Disabled),
        "invalid" => Ok(RegistryStatus::Invalid),
        _ => Err(error_response_coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("unknown persisted {entity} status: {raw}"),
            code,
        )),
    }
}

pub(crate) fn canonical_json_string(value: &Value) -> String {
    astra_core::canonical_json_string(value)
}

pub(crate) fn canonical_serialize<T: Serialize>(
    value: &T,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let json = serde_json::to_value(value).map_err(|_| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            "payload must be valid JSON",
            "registry_payload_invalid",
        )
    })?;
    Ok(canonical_json_string(&json))
}

pub(crate) fn exact_non_empty_string(
    field: &'static str,
    value: &str,
    code: &'static str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!(
                "{field} must be a non-empty exact string without leading/trailing whitespace or control characters"
            ),
            code,
        ));
    }
    Ok(())
}

pub(crate) fn exact_non_empty_markdown_string(
    field: &'static str,
    value: &str,
    code: &'static str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().any(|ch| ch.is_control() && ch != '\n')
    {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!(
                "{field} must be a non-empty exact markdown string without leading/trailing whitespace or control characters other than line feed"
            ),
            code,
        ));
    }
    Ok(())
}

pub(crate) fn exact_id_string(
    field: &'static str,
    value: &str,
    max_len: usize,
    code: &'static str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    exact_non_empty_string(field, value, code)?;
    if value.len() > max_len || value.contains('/') || value.contains('\\') {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{field} must be at most {max_len} bytes and must not contain path separators"),
            code,
        ));
    }
    Ok(())
}

pub fn validate_registered_endpoint_url(
    field: &'static str,
    value: &str,
    code: &'static str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    exact_non_empty_string(field, value, code)?;
    let parsed = reqwest::Url::parse(value).map_err(|_| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{field} must be an absolute http or https URL"),
            code,
        )
    })?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                format!("{field} must use http or https"),
                code,
            ));
        }
    }
    if parsed.host_str().is_none() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{field} must be an absolute http or https URL"),
            code,
        ));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{field} must not contain userinfo, query, or fragment"),
            code,
        ));
    }
    Ok(())
}

pub(crate) fn reject_secret_like_json(
    field: &'static str,
    value: &Value,
    code: &'static str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    fn is_secret_key(key: &str) -> bool {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "model_policy"
                | "selected_model"
                | "model_gateway"
                | "authorization"
                | "auth_token"
                | "api_key"
                | "token"
                | "secret"
                | "password"
                | "cookie"
                | "set-cookie"
                | "credential"
                | "credentials"
                | "headers"
                | "runtime_token"
                | "provider_api_key"
                | "provider_base_url"
                | "client_workspace_id"
                | "client_user_id"
                | "caller_workspace_id"
                | "caller_user_id"
                | "allowed_tools"
                | "allowed_skills"
                | "allowed_models"
                | "tool_specs"
                | "tool_schemas"
                | "model_schemas"
                | "discovered_tools"
                | "discovered_skills"
                | "discovery_results"
                | "runtime_discovery"
        )
    }

    fn walk(
        path: &str,
        value: &Value,
        code: &'static str,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let next_path = format!("{path}.{key}");
                    if is_secret_key(key) {
                        return Err(error_response_coded(
                            StatusCode::BAD_REQUEST,
                            format!("{next_path} must not contain secret or runtime-scope fields"),
                            code,
                        ));
                    }
                    walk(&next_path, child, code)?;
                }
            }
            Value::Array(items) => {
                for (idx, child) in items.iter().enumerate() {
                    walk(&format!("{path}[{idx}]"), child, code)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    walk(field, value, code)
}
