use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use astra_core::{ErrorResponse, error_response_coded};

use crate::server::runtime_mcp::redact_known_secrets;
use crate::skills::manifest::{ExecutionContext, SkillSourceKind, TrustTier};
use crate::turn::skill_tool::{ResolvedSkill, SkillResolver, SkillToolInfo};

const SKILL_GATEWAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SKILL_GATEWAY_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SKILL_GATEWAY_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const SKILL_LIST_REQUEST_ID: &str = "astra-agent-binding-skills-list";
const SKILL_READ_REQUEST_ID: &str = "astra-agent-binding-skills-read";

#[derive(Serialize)]
struct SkillListRequest<'a> {
    jsonrpc: &'a str,
    id: &'a str,
    method: &'a str,
}

#[derive(Serialize)]
struct SkillReadRequest<'a> {
    jsonrpc: &'a str,
    id: &'a str,
    method: &'a str,
    params: SkillReadParams<'a>,
}

#[derive(Serialize)]
struct SkillReadParams<'a> {
    id: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillListJsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(default)]
    result: Option<SkillListResponse>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillListResponse {
    skills: Vec<DiscoveredSkill>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillReadJsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(default)]
    result: Option<SkillReadResponse>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillReadResponse {
    skill: ReadSkill,
}

#[derive(Deserialize)]
struct ReadSkill {
    id: String,
    instruction: ReadSkillInstruction,
}

#[derive(Deserialize)]
struct ReadSkillInstruction {
    body: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveredSkill {
    name: String,
    description: String,
    #[serde(default)]
    when_to_use: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    input_schema: Option<Value>,
    #[serde(default)]
    output_schema: Option<Value>,
}

#[derive(Clone)]
struct AgentBindingSkillEntry {
    info: SkillToolInfo,
    resolved: ResolvedSkill,
}

#[derive(Clone)]
struct AgentBindingSkillResolver {
    server_id: String,
    endpoint_url: String,
    authorization: String,
    skills: Vec<AgentBindingSkillEntry>,
    by_name: HashMap<String, usize>,
}

impl AgentBindingSkillResolver {
    fn entry(&self, name: &str) -> Result<&AgentBindingSkillEntry, crate::skills::SkillError> {
        let key = normalize_skill_lookup_key(name);
        let Some(index) = self.by_name.get(&key) else {
            return Err(crate::skills::SkillError::NotFound(format!(
                "Agent Binding skill not found: {name}"
            )));
        };
        Ok(&self.skills[*index])
    }

    async fn read_skill_instructions(
        &self,
        skill_id: &str,
    ) -> Result<String, crate::skills::SkillError> {
        let _permit = crate::capability_endpoint_pool::try_acquire_endpoint_permit(
            &self.endpoint_url,
        )
        .map_err(|detail| {
            crate::skills::SkillError::LoadFailed(format!(
                "Agent Binding skills/read capacity unavailable for server '{}' and skill '{}': {}",
                self.server_id, skill_id, detail
            ))
        })?;
        let response = skill_gateway_http_client()
            .post(&self.endpoint_url)
            .header(reqwest::header::AUTHORIZATION, &self.authorization)
            .json(&SkillReadRequest {
                jsonrpc: "2.0",
                id: SKILL_READ_REQUEST_ID,
                method: "skills/read",
                params: SkillReadParams { id: skill_id },
            })
            .send()
            .await
            .map_err(|error| {
                let detail = redact_skill_gateway_error(&error.to_string(), &self.authorization);
                crate::skills::SkillError::LoadFailed(format!(
                    "Agent Binding skills/read request failed for server '{}' and skill '{}': {}",
                    self.server_id, skill_id, detail
                ))
            })?;

        let status = response.status();
        let body = read_response_body_limited(response, "skills/read")
            .await
            .map_err(|(status, error)| {
                crate::skills::SkillError::LoadFailed(format!(
                    "Agent Binding skills/read response failed for server '{}' and skill '{}' (HTTP {}): {}",
                    self.server_id,
                    skill_id,
                    status.as_u16(),
                    error.0.detail
                ))
            })?;
        if !status.is_success() {
            let detail = redact_skill_gateway_error(
                String::from_utf8_lossy(&body).trim(),
                &self.authorization,
            );
            return Err(crate::skills::SkillError::LoadFailed(
                if detail.is_empty() {
                    format!(
                        "Agent Binding skills/read failed for server '{}' and skill '{}': HTTP {}",
                        self.server_id,
                        skill_id,
                        status.as_u16()
                    )
                } else {
                    format!(
                        "Agent Binding skills/read failed for server '{}' and skill '{}': HTTP {}: {}",
                        self.server_id,
                        skill_id,
                        status.as_u16(),
                        detail
                    )
                },
            ));
        }
        decode_skill_read_response(&body, skill_id)
    }
}

#[async_trait::async_trait]
impl SkillResolver for AgentBindingSkillResolver {
    fn resolve(&self, name: &str) -> Result<ResolvedSkill, crate::skills::SkillError> {
        let entry = self.entry(name)?;
        Err(crate::skills::SkillError::LoadFailed(format!(
            "Agent Binding skill '{}' requires asynchronous skills/read resolution",
            entry.resolved.name
        )))
    }

    async fn resolve_for_execution(
        &self,
        name: &str,
    ) -> Result<ResolvedSkill, crate::skills::SkillError> {
        let mut resolved = self.entry(name)?.resolved.clone();
        resolved.instructions = self.read_skill_instructions(&resolved.name).await?;
        Ok(resolved)
    }

    fn available_skills(&self) -> Vec<SkillToolInfo> {
        self.skills.iter().map(|entry| entry.info.clone()).collect()
    }
}

fn skill_gateway_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        astra_core::net::build_internal_http_client(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(SKILL_GATEWAY_CONNECT_TIMEOUT)
                .timeout(SKILL_GATEWAY_REQUEST_TIMEOUT),
            "agent binding skill gateway client",
        )
    })
}

fn skill_error(
    status: StatusCode,
    detail: impl Into<String>,
    code: &'static str,
) -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(status, detail, code)
}

fn validate_exact_string(
    field: &'static str,
    value: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(skill_error(
            StatusCode::BAD_GATEWAY,
            format!("Agent Binding skills/list field {field} must be a non-empty exact string"),
            "agent_binding_schema_invalid",
        ));
    }
    Ok(())
}

fn validate_optional_exact_string(
    field: &'static str,
    value: Option<&String>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if let Some(value) = value {
        validate_exact_string(field, value)?;
    }
    Ok(())
}

fn validate_skill_endpoint(url: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    astra_services::validate_registered_endpoint_url(
        "agent_binding.capability_server.endpoint_url",
        url,
        "agent_binding_capability_ref_invalid",
    )
}

fn skill_gateway_secret_value(authorization: &str) -> Value {
    json!({
        "authorization": authorization,
        "token": authorization.strip_prefix("Bearer ").unwrap_or(authorization),
    })
}

fn redact_skill_gateway_error(raw: &str, authorization: &str) -> String {
    redact_known_secrets(raw, &skill_gateway_secret_value(authorization))
}

fn decode_skill_list_response(
    body: &[u8],
) -> Result<SkillListResponse, (StatusCode, Json<ErrorResponse>)> {
    let response: SkillListJsonRpcResponse = serde_json::from_slice(body).map_err(|error| {
        skill_error(
            StatusCode::BAD_GATEWAY,
            format!("Agent Binding skills/list response is invalid: {error}"),
            "agent_binding_schema_invalid",
        )
    })?;
    if response.jsonrpc != "2.0" {
        return Err(skill_error(
            StatusCode::BAD_GATEWAY,
            "Agent Binding skills/list JSON-RPC response jsonrpc must be 2.0",
            "agent_binding_schema_invalid",
        ));
    }
    if response.id != Value::String(SKILL_LIST_REQUEST_ID.to_string()) {
        return Err(skill_error(
            StatusCode::BAD_GATEWAY,
            "Agent Binding skills/list JSON-RPC response id mismatch",
            "agent_binding_schema_invalid",
        ));
    }
    if let Some(error) = response.error {
        let code = error
            .get("code")
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown skill JSON-RPC error");
        return Err(skill_error(
            StatusCode::BAD_GATEWAY,
            format!("Agent Binding skill JSON-RPC error {code}: {message}"),
            "agent_binding_discovery_failed",
        ));
    }
    response.result.ok_or_else(|| {
        skill_error(
            StatusCode::BAD_GATEWAY,
            "Agent Binding skills/list JSON-RPC response missing result",
            "agent_binding_schema_invalid",
        )
    })
}

fn decode_skill_read_response(
    body: &[u8],
    expected_skill_id: &str,
) -> Result<String, crate::skills::SkillError> {
    let response: SkillReadJsonRpcResponse = serde_json::from_slice(body).map_err(|error| {
        crate::skills::SkillError::ParseFailed(format!(
            "Agent Binding skills/read response for '{expected_skill_id}' is invalid: {error}"
        ))
    })?;
    if response.jsonrpc != "2.0" {
        return Err(crate::skills::SkillError::ParseFailed(format!(
            "Agent Binding skills/read response for '{expected_skill_id}' must use JSON-RPC 2.0"
        )));
    }
    if response.id != Value::String(SKILL_READ_REQUEST_ID.to_string()) {
        return Err(crate::skills::SkillError::ParseFailed(format!(
            "Agent Binding skills/read response id mismatch for '{expected_skill_id}'"
        )));
    }
    if let Some(error) = response.error {
        let code = error
            .get("code")
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown skill JSON-RPC error");
        return Err(crate::skills::SkillError::LoadFailed(format!(
            "Agent Binding skills/read JSON-RPC error for '{expected_skill_id}' ({code}): {message}"
        )));
    }
    let result = response.result.ok_or_else(|| {
        crate::skills::SkillError::ParseFailed(format!(
            "Agent Binding skills/read response missing result for '{expected_skill_id}'"
        ))
    })?;
    if result.skill.id != expected_skill_id {
        return Err(crate::skills::SkillError::ParseFailed(format!(
            "Agent Binding skills/read returned skill '{}' for requested skill '{expected_skill_id}'",
            result.skill.id
        )));
    }
    if result.skill.instruction.body.trim().is_empty() {
        return Err(crate::skills::SkillError::ParseFailed(format!(
            "Agent Binding skills/read returned an empty instruction for '{expected_skill_id}'"
        )));
    }
    Ok(result.skill.instruction.body)
}

async fn read_response_body_limited(
    response: reqwest::Response,
    method: &'static str,
) -> Result<Vec<u8>, (StatusCode, Json<ErrorResponse>)> {
    use futures_util::StreamExt;

    if let Some(content_length) = response.content_length()
        && content_length > SKILL_GATEWAY_MAX_RESPONSE_BYTES as u64
    {
        return Err(skill_error(
            StatusCode::BAD_GATEWAY,
            format!("Agent Binding {method} response body is too large"),
            "agent_binding_schema_invalid",
        ));
    }

    let mut stream = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            skill_error(
                StatusCode::BAD_GATEWAY,
                format!("Agent Binding {method} response read failed: {error}"),
                "agent_binding_discovery_failed",
            )
        })?;
        let remaining = SKILL_GATEWAY_MAX_RESPONSE_BYTES.saturating_sub(collected.len());
        if chunk.len() > remaining {
            return Err(skill_error(
                StatusCode::BAD_GATEWAY,
                format!("Agent Binding {method} response body exceeds size limit"),
                "agent_binding_schema_invalid",
            ));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
}

fn normalize_skill_lookup_key(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn validate_discovered_skill(
    skill: &DiscoveredSkill,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_exact_string("skills[].name", &skill.name)?;
    validate_exact_string("skills[].description", &skill.description)?;
    validate_optional_exact_string("skills[].when_to_use", skill.when_to_use.as_ref())?;
    validate_optional_exact_string("skills[].model", skill.model.as_ref())?;
    for alias in &skill.aliases {
        validate_exact_string("skills[].aliases[]", alias)?;
    }
    if let Some(category) = &skill.category {
        validate_exact_string("skills[].category", category)?;
    }
    for tag in &skill.tags {
        validate_exact_string("skills[].tags[]", tag)?;
    }
    for tool in &skill.allowed_tools {
        validate_exact_string("skills[].allowed_tools[]", tool)?;
    }
    Ok(())
}

fn build_resolver(
    server_id: &str,
    endpoint_url: &str,
    authorization: &str,
    skills: Vec<DiscoveredSkill>,
) -> Result<Option<Arc<dyn SkillResolver>>, (StatusCode, Json<ErrorResponse>)> {
    if skills.is_empty() {
        return Ok(None);
    }

    let mut entries = Vec::with_capacity(skills.len());
    let mut by_name = HashMap::new();
    let mut seen = HashSet::new();

    for skill in skills {
        validate_discovered_skill(&skill)?;
        for candidate in std::iter::once(&skill.name).chain(skill.aliases.iter()) {
            let normalized = normalize_skill_lookup_key(candidate);
            if !seen.insert(normalized.clone()) {
                return Err(skill_error(
                    StatusCode::BAD_GATEWAY,
                    format!("duplicate Agent Binding skill name or alias: {candidate}"),
                    "agent_binding_schema_invalid",
                ));
            }
        }

        let info = SkillToolInfo {
            name: skill.name.clone(),
            description: skill.description.clone(),
            when_to_use: skill.when_to_use.clone(),
            source: SkillSourceKind::Plugin,
            aliases: skill.aliases.clone(),
            category: skill.category.clone(),
            tags: skill.tags.clone(),
        };
        let resolved = ResolvedSkill {
            name: skill.name.clone(),
            instructions: String::new(),
            max_tokens: skill.max_tokens,
            allowed_tools: skill.allowed_tools.clone(),
            execution_context: ExecutionContext::Inline,
            hooks: Default::default(),
            skill_dir: None,
            source: SkillSourceKind::Plugin,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: skill.input_schema.clone(),
            output_schema: skill.output_schema.clone(),
            remote_url: None,
            forward_headers: Vec::new(),
            required_headers: Vec::new(),
            aliases: skill.aliases.clone(),
            effort: None,
            agent_type: None,
            trust_tier: TrustTier::Verified,
        };
        let index = entries.len();
        by_name.insert(normalize_skill_lookup_key(&skill.name), index);
        for alias in &skill.aliases {
            by_name.insert(normalize_skill_lookup_key(alias), index);
        }
        entries.push(AgentBindingSkillEntry { info, resolved });
    }

    Ok(Some(Arc::new(AgentBindingSkillResolver {
        server_id: server_id.to_string(),
        endpoint_url: endpoint_url.to_string(),
        authorization: authorization.to_string(),
        skills: entries,
        by_name,
    })))
}

pub(crate) async fn prepare_agent_binding_skill_resolver(
    server_id: &str,
    endpoint_url: &str,
    authorization: &str,
) -> Result<Option<Arc<dyn SkillResolver>>, (StatusCode, Json<ErrorResponse>)> {
    validate_skill_endpoint(endpoint_url)?;
    let _permit = crate::capability_endpoint_pool::try_acquire_endpoint_permit(endpoint_url)
        .map_err(|detail| {
            skill_error(
                StatusCode::TOO_MANY_REQUESTS,
                detail,
                "agent_binding_discovery_failed",
            )
        })?;
    let response = skill_gateway_http_client()
        .post(endpoint_url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .json(&SkillListRequest {
            jsonrpc: "2.0",
            id: SKILL_LIST_REQUEST_ID,
            method: "skills/list",
        })
        .send()
        .await
        .map_err(|error| {
            let detail = redact_skill_gateway_error(&error.to_string(), authorization);
            skill_error(
                StatusCode::BAD_GATEWAY,
                format!(
                    "Agent Binding skill discovery failed for server '{}': {}",
                    server_id, detail
                ),
                "agent_binding_discovery_failed",
            )
        })?;

    let status = response.status();
    let body = read_response_body_limited(response, "skills/list").await?;
    if !status.is_success() {
        let detail =
            redact_skill_gateway_error(String::from_utf8_lossy(&body).trim(), authorization);
        return Err(skill_error(
            StatusCode::BAD_GATEWAY,
            if detail.is_empty() {
                format!(
                    "Agent Binding skill discovery failed for server '{}': HTTP {}",
                    server_id,
                    status.as_u16()
                )
            } else {
                format!(
                    "Agent Binding skill discovery failed for server '{}': HTTP {}: {}",
                    server_id,
                    status.as_u16(),
                    detail
                )
            },
            "agent_binding_discovery_failed",
        ));
    }

    let response = decode_skill_list_response(&body)?;
    build_resolver(server_id, endpoint_url, authorization, response.skills)
}

pub(crate) async fn prepare_runtime_skill_resolver(
    server_id: &str,
    endpoint_url: &str,
    authorization: &str,
) -> Result<Option<Arc<dyn SkillResolver>>, (StatusCode, Json<ErrorResponse>)> {
    prepare_agent_binding_skill_resolver(server_id, endpoint_url, authorization).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn build_resolver_maps_discovered_skill_to_lazy_catalog_entry() {
        let resolver = build_resolver(
            "skills",
            "https://skills.example.com/api/v1/skills",
            "Bearer runtime-grant",
            vec![DiscoveredSkill {
                name: "query-db".to_string(),
                description: "Query a database".to_string(),
                when_to_use: Some("when the user asks for data".to_string()),
                aliases: vec!["db".to_string()],
                category: Some("data".to_string()),
                tags: vec!["sql".to_string()],
                model: Some("gpt-4.1".to_string()),
                max_tokens: Some(2048),
                allowed_tools: vec!["mcp__tools__query".to_string()],
                input_schema: None,
                output_schema: None,
            }],
        )
        .expect("valid resolver")
        .expect("non-empty resolver");

        let names = resolver.available_skills();
        assert_eq!(names[0].name, "query-db");
        let error = resolver
            .resolve("db")
            .expect_err("remote skill content must not appear resolved before skills/read");
        assert!(
            error
                .to_string()
                .contains("requires asynchronous skills/read resolution")
        );
    }

    #[test]
    fn build_resolver_allows_empty_skill_list() {
        let resolver = build_resolver(
            "skills",
            "https://skills.example.com/api/v1/skills",
            "Bearer runtime-grant",
            Vec::new(),
        )
        .expect("empty Agent Binding skill discovery should be allowed");
        assert!(resolver.is_none());
    }

    #[test]
    fn decode_skill_read_response_rejects_a_different_skill() {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": SKILL_READ_REQUEST_ID,
            "result": {
                "skill": {
                    "id": "pdf",
                    "instruction": {"body": "PDF instructions"}
                }
            }
        }))
        .expect("serialize response");

        let error = decode_skill_read_response(&body, "xlsx")
            .expect_err("skills/read must return the exact selected skill");
        assert!(
            error
                .to_string()
                .contains("returned skill 'pdf' for requested skill 'xlsx'")
        );
    }

    #[test]
    fn skill_endpoint_validator_uses_registered_capability_rules() {
        for endpoint_url in [
            "wss://skills.example.test/list",
            "https://user:pass@skills.example.test/list",
            "https://skills.example.test/list?token=secret",
            "https://skills.example.test/list#fragment",
        ] {
            let err = validate_skill_endpoint(endpoint_url)
                .expect_err("registered skill endpoint URL must be strict");
            assert_eq!(err.0, StatusCode::BAD_REQUEST, "{endpoint_url}");
            assert_eq!(
                err.1.0.error_code.as_deref(),
                Some("agent_binding_capability_ref_invalid"),
                "{endpoint_url}"
            );
        }
    }

    #[tokio::test]
    async fn prepare_agent_binding_skill_resolver_sends_runtime_auth() {
        use axum::{Router, extract::State, http::HeaderMap, routing::post};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        #[derive(Default)]
        struct Capture {
            authorization: Mutex<Option<String>>,
            body: Mutex<Option<Value>>,
        }

        async fn handler(
            State(capture): State<Arc<Capture>>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            *capture.authorization.lock().await = headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string);
            *capture.body.lock().await = Some(body);
            Json(json!({
                "jsonrpc": "2.0",
                "id": SKILL_LIST_REQUEST_ID,
                "result": {
                    "skills": []
                }
            }))
        }

        let capture = Arc::new(Capture::default());
        let app = Router::new()
            .route("/skills", post(handler))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoint = format!("http://{addr}/skills");

        let resolver =
            prepare_agent_binding_skill_resolver("skills", &endpoint, "Bearer runtime-grant")
                .await
                .expect("empty Agent Binding skill discovery should be allowed");

        assert!(resolver.is_none());
        assert_eq!(
            capture.authorization.lock().await.as_deref(),
            Some("Bearer runtime-grant")
        );
        assert_eq!(
            capture.body.lock().await.as_ref(),
            Some(&json!({
                "jsonrpc": "2.0",
                "id": SKILL_LIST_REQUEST_ID,
                "method": "skills/list"
            }))
        );
        server.abort();
    }

    #[tokio::test]
    async fn agent_binding_skill_execution_reads_full_skill_only_after_selection() {
        use axum::{Router, extract::State, http::HeaderMap, routing::post};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        #[derive(Default)]
        struct Capture {
            authorizations: Mutex<Vec<String>>,
            bodies: Mutex<Vec<Value>>,
        }

        async fn handler(
            State(capture): State<Arc<Capture>>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            capture.authorizations.lock().await.push(
                headers
                    .get(reqwest::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            );
            capture.bodies.lock().await.push(body.clone());
            match body.get("method").and_then(Value::as_str) {
                Some("skills/list") => Json(json!({
                    "jsonrpc": "2.0",
                    "id": SKILL_LIST_REQUEST_ID,
                    "result": {
                        "skills": [{
                            "name": "xlsx",
                            "description": "Work with spreadsheets",
                            "when_to_use": "when the user provides an XLSX file",
                            "aliases": ["spreadsheet"],
                            "allowed_tools": ["python"]
                        }]
                    }
                })),
                Some("skills/read") => Json(json!({
                    "jsonrpc": "2.0",
                    "id": SKILL_READ_REQUEST_ID,
                    "result": {
                        "skill": {
                            "id": "xlsx",
                            "name": "XLSX",
                            "instruction": {
                                "body": "Use openpyxl and verify the generated workbook."
                            }
                        }
                    }
                })),
                other => panic!("unexpected method: {other:?}"),
            }
        }

        let capture = Arc::new(Capture::default());
        let app = Router::new()
            .route("/skills", post(handler))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoint = format!("http://{addr}/skills");

        let resolver =
            prepare_agent_binding_skill_resolver("moi-skills", &endpoint, "Bearer runtime-grant")
                .await
                .expect("skill discovery should succeed")
                .expect("skill resolver");
        assert_eq!(
            capture.bodies.lock().await.as_slice(),
            &[json!({
                "jsonrpc": "2.0",
                "id": SKILL_LIST_REQUEST_ID,
                "method": "skills/list"
            })],
            "preparation must fetch summaries only"
        );

        let result = crate::turn::skill_tool::execute_skill_direct(
            resolver.as_ref(),
            None,
            "spreadsheet",
            "",
            None,
            &crate::turn::skill_tool::SkillContext::default(),
        )
        .await;
        assert!(
            result.success,
            "selected skill should load: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("Use openpyxl and verify the generated workbook.")
        );
        assert_eq!(
            capture.bodies.lock().await.as_slice(),
            &[
                json!({
                    "jsonrpc": "2.0",
                    "id": SKILL_LIST_REQUEST_ID,
                    "method": "skills/list"
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": SKILL_READ_REQUEST_ID,
                    "method": "skills/read",
                    "params": {"id": "xlsx"}
                })
            ]
        );
        assert_eq!(
            capture.authorizations.lock().await.as_slice(),
            &["Bearer runtime-grant", "Bearer runtime-grant"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn prepare_agent_binding_skill_resolver_rejects_json_rpc_error_response() {
        use axum::{Router, http::StatusCode as AxumStatusCode, routing::post};

        let app = Router::new().route(
            "/skills",
            post(|| async {
                (
                    AxumStatusCode::OK,
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": SKILL_LIST_REQUEST_ID,
                        "error": {
                            "code": -32602,
                            "message": "invalid params"
                        }
                    })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoint = format!("http://{addr}/skills");

        let err =
            match prepare_agent_binding_skill_resolver("skills", &endpoint, "Bearer runtime-grant")
                .await
            {
                Ok(_) => panic!("JSON-RPC error skill discovery must fail"),
                Err(err) => err,
            };

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("agent_binding_discovery_failed")
        );
        assert!(err.1.0.detail.contains("invalid params"));
        server.abort();
    }

    #[tokio::test]
    async fn prepare_agent_binding_skill_resolver_redacts_runtime_auth_from_error_body() {
        use axum::{Router, http::StatusCode as AxumStatusCode, routing::post};

        let app = Router::new().route(
            "/skills",
            post(|| async {
                (
                    AxumStatusCode::INTERNAL_SERVER_ERROR,
                    "upstream echoed Bearer runtime-grant and runtime-grant".to_string(),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoint = format!("http://{addr}/skills");

        let err =
            match prepare_agent_binding_skill_resolver("skills", &endpoint, "Bearer runtime-grant")
                .await
            {
                Ok(_) => panic!("non-2xx skill discovery must fail"),
                Err(err) => err,
            };

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("agent_binding_discovery_failed")
        );
        assert!(!err.1.0.detail.contains("Bearer runtime-grant"));
        assert!(!err.1.0.detail.contains("runtime-grant"));
        assert!(err.1.0.detail.contains("[REDACTED]"));
        server.abort();
    }

    #[tokio::test]
    async fn prepare_agent_binding_skill_resolver_redacts_short_runtime_auth_from_error_body() {
        use axum::{Router, http::StatusCode as AxumStatusCode, routing::post};

        let app = Router::new().route(
            "/skills",
            post(|| async {
                (
                    AxumStatusCode::INTERNAL_SERVER_ERROR,
                    "upstream echoed Bearer abc and abc".to_string(),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoint = format!("http://{addr}/skills");

        let err =
            match prepare_agent_binding_skill_resolver("skills", &endpoint, "Bearer abc").await {
                Ok(_) => panic!("non-2xx skill discovery must fail"),
                Err(err) => err,
            };

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("agent_binding_discovery_failed")
        );
        assert!(!err.1.0.detail.contains("Bearer abc"));
        assert!(!err.1.0.detail.contains("abc"));
        assert!(err.1.0.detail.contains("[REDACTED]"));
        server.abort();
    }

    #[test]
    fn build_resolver_rejects_duplicate_alias_surface() {
        let err = match build_resolver(
            "skills",
            "https://skills.example.com/api/v1/skills",
            "Bearer runtime-grant",
            vec![
                DiscoveredSkill {
                    name: "query-db".to_string(),
                    description: "Query".to_string(),
                    when_to_use: None,
                    aliases: vec!["data".to_string()],
                    category: None,
                    tags: Vec::new(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: Vec::new(),
                    input_schema: None,
                    output_schema: None,
                },
                DiscoveredSkill {
                    name: "data".to_string(),
                    description: "Duplicate".to_string(),
                    when_to_use: None,
                    aliases: Vec::new(),
                    category: None,
                    tags: Vec::new(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: Vec::new(),
                    input_schema: None,
                    output_schema: None,
                },
            ],
        ) {
            Ok(_) => panic!("duplicate names and aliases fail"),
            Err(err) => err,
        };
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("agent_binding_schema_invalid")
        );
    }
}
