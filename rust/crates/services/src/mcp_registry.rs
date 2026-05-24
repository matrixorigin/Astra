use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, query};
use std::{collections::HashSet, fmt, sync::Arc};

use crate::auth::FernetTokenEncryptor;
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response_coded,
    internal_error, is_duplicate_key_error,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerRequestData {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub transport: String,
    pub url: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct McpBindingRequestData {
    pub alias: String,
    pub key_value: serde_json::Value,
    #[serde(default)]
    pub comment: Option<String>,
}

impl fmt::Debug for McpBindingRequestData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpBindingRequestData")
            .field("alias", &self.alias)
            .field("key_value", &"[REDACTED]")
            .field("comment", &self.comment)
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct McpRegisterRequestData {
    pub server: McpServerRequestData,
    pub binding: McpBindingRequestData,
}

impl fmt::Debug for McpRegisterRequestData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpRegisterRequestData")
            .field("server", &self.server)
            .field("binding", &self.binding)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpDiscoveredToolData {
    pub tool_name: String,
    pub public_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema_json: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema_json: Option<serde_json::Value>,
    pub schema_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRegisteredToolRecord {
    pub tool_name: String,
    pub public_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRegisterRecord {
    pub mcp_id: i64,
    pub binding_id: i64,
    pub server_name: String,
    pub binding_alias: String,
    pub tools: Vec<McpRegisteredToolRecord>,
}

#[derive(Clone, PartialEq)]
pub struct McpRuntimeBindingRecord {
    pub binding_id: i64,
    pub mcp_id: i64,
    pub server_name: String,
    pub server_description: Option<String>,
    pub transport: String,
    pub url: String,
    pub binding_alias: String,
    pub key_value: serde_json::Value,
    pub tools: Vec<McpDiscoveredToolData>,
}

impl fmt::Debug for McpRuntimeBindingRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpRuntimeBindingRecord")
            .field("binding_id", &self.binding_id)
            .field("mcp_id", &self.mcp_id)
            .field("server_name", &self.server_name)
            .field("server_description", &self.server_description)
            .field("transport", &self.transport)
            .field("url", &self.url)
            .field("binding_alias", &self.binding_alias)
            .field("key_value", &"[REDACTED]")
            .field("tools", &self.tools)
            .finish()
    }
}

pub fn mcp_schema_hash(parts: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(parts.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

#[async_trait]
pub trait McpRegistryService: Send + Sync {
    async fn upsert_discovered_binding(
        &self,
        owner_user_id: String,
        request: McpRegisterRequestData,
        discovered_tools: Vec<McpDiscoveredToolData>,
    ) -> Result<McpRegisterRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn load_runtime_bindings(
        &self,
        owner_user_id: String,
        binding_ids: &[i64],
    ) -> Result<Vec<McpRuntimeBindingRecord>, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Default)]
pub struct UnconfiguredMcpRegistryService;

#[async_trait]
impl McpRegistryService for UnconfiguredMcpRegistryService {
    async fn upsert_discovered_binding(
        &self,
        _owner_user_id: String,
        _request: McpRegisterRequestData,
        _discovered_tools: Vec<McpDiscoveredToolData>,
    ) -> Result<McpRegisterRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "MCP registry service not configured",
            "mcp_registry_unconfigured",
        ))
    }

    async fn load_runtime_bindings(
        &self,
        _owner_user_id: String,
        _binding_ids: &[i64],
    ) -> Result<Vec<McpRuntimeBindingRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "MCP registry service not configured",
            "mcp_registry_unconfigured",
        ))
    }
}

#[derive(Clone)]
pub struct DatabaseMcpRegistryService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
    encryptor: Arc<FernetTokenEncryptor>,
}

impl DatabaseMcpRegistryService {
    pub fn new(matrixone: MatrixOneSettings, encryptor: Arc<FernetTokenEncryptor>) -> Self {
        Self {
            matrixone,
            pool: None,
            encryptor,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<MySql>, sqlx::Error> {
        if let Some(ref pool) = self.pool {
            return Ok(pool.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }

    fn encrypted_key_value(
        &self,
        key_value: &serde_json::Value,
    ) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
        let plaintext = serde_json::to_string(key_value).map_err(|_| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                "MCP binding key_value must be valid JSON",
                "mcp_key_value_invalid",
            )
        })?;
        self.encryptor.encrypt(&plaintext).map_err(|_| {
            error_response_coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to encrypt MCP credential",
                "mcp_credential_encrypt_failed",
            )
        })
    }

    fn decrypt_key_value(
        &self,
        encrypted: &str,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        let plaintext = self.encryptor.decrypt(encrypted).map_err(|_| {
            error_response_coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to decrypt MCP credential",
                "mcp_credential_decrypt_failed",
            )
        })?;
        serde_json::from_str(&plaintext).map_err(|_| {
            error_response_coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stored MCP credential is not valid JSON",
                "mcp_credential_invalid",
            )
        })
    }
}

fn validate_upsert_input(
    owner_user_id: &str,
    request: &McpRegisterRequestData,
    discovered_tools: &[McpDiscoveredToolData],
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if owner_user_id.trim().is_empty() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "owner_user_id must not be empty",
            "mcp_owner_invalid",
        ));
    }
    if request.server.name.trim().is_empty() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "server.name must not be empty",
            "mcp_server_invalid",
        ));
    }
    if request.server.transport.trim().is_empty() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "server.transport must not be empty",
            "mcp_server_invalid",
        ));
    }
    if request.server.url.trim().is_empty() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "server.url must not be empty",
            "mcp_server_invalid",
        ));
    }
    if request.binding.alias.trim().is_empty() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "binding.alias must not be empty",
            "mcp_binding_invalid",
        ));
    }
    if !request.binding.key_value.is_object() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "binding.key_value must be a JSON object",
            "mcp_key_value_invalid",
        ));
    }
    let mut public_names = HashSet::new();
    let mut tool_names = HashSet::new();
    for tool in discovered_tools {
        if tool.tool_name.trim().is_empty() || tool.public_name.trim().is_empty() {
            return Err(error_response_coded(
                StatusCode::BAD_GATEWAY,
                "MCP discovery returned a tool without a valid name",
                "mcp_discovery_failed",
            ));
        }
        if tool.schema_hash.trim().is_empty() {
            return Err(error_response_coded(
                StatusCode::BAD_GATEWAY,
                "MCP discovery returned a tool without a schema hash",
                "mcp_discovery_failed",
            ));
        }
        if !tool_names.insert(tool.tool_name.as_str()) {
            return Err(error_response_coded(
                StatusCode::CONFLICT,
                format!("duplicate MCP tool name: {}", tool.tool_name),
                "mcp_tool_name_conflict",
            ));
        }
        if !public_names.insert(tool.public_name.as_str()) {
            return Err(error_response_coded(
                StatusCode::CONFLICT,
                format!("duplicate MCP public tool name: {}", tool.public_name),
                "mcp_public_name_conflict",
            ));
        }
    }
    Ok(())
}

fn parse_json_column(raw: Option<String>) -> Option<serde_json::Value> {
    raw.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
}

fn canonical_binding_ids(
    binding_ids: &[i64],
) -> Result<Vec<i64>, (StatusCode, Json<ErrorResponse>)> {
    let mut unique_ids = Vec::new();
    let mut seen = HashSet::new();
    for id in binding_ids {
        if *id <= 0 {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "mcp_binding_ids must contain positive integers",
                "mcp_binding_invalid",
            ));
        }
        if seen.insert(*id) {
            unique_ids.push(*id);
        }
    }
    unique_ids.sort_unstable();
    Ok(unique_ids)
}

#[async_trait]
impl McpRegistryService for DatabaseMcpRegistryService {
    async fn upsert_discovered_binding(
        &self,
        owner_user_id: String,
        request: McpRegisterRequestData,
        discovered_tools: Vec<McpDiscoveredToolData>,
    ) -> Result<McpRegisterRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_upsert_input(&owner_user_id, &request, &discovered_tools)?;

        let pool = self.get_pool().await.map_err(internal_error)?;
        let encrypted_key_value = self.encrypted_key_value(&request.binding.key_value)?;
        let mut tx = pool.begin().await.map_err(internal_error)?;

        query(
            "INSERT INTO mcp_servers \
             (owner_user_id, name, description, transport, url, is_active, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 1, NOW(6), NOW(6)) \
             ON DUPLICATE KEY UPDATE \
             description = VALUES(description), transport = VALUES(transport), url = VALUES(url), \
             is_active = 1, updated_at = NOW(6)",
        )
        .bind(&owner_user_id)
        .bind(request.server.name.trim())
        .bind(request.server.description.as_deref())
        .bind(request.server.transport.trim())
        .bind(request.server.url.trim())
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        let server_row = query(
            "SELECT id FROM mcp_servers WHERE owner_user_id = ? AND name = ? AND is_active = 1",
        )
        .bind(&owner_user_id)
        .bind(request.server.name.trim())
        .fetch_one(&mut *tx)
        .await
        .map_err(internal_error)?;
        let mcp_id: i64 = server_row.try_get("id").map_err(internal_error)?;

        let existing_binding = query(
            "SELECT id, mcp_id FROM mcp_bindings \
             WHERE owner_user_id = ? AND alias = ? AND is_active = 1",
        )
        .bind(&owner_user_id)
        .bind(request.binding.alias.trim())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal_error)?;

        let binding_id = if let Some(row) = existing_binding {
            let existing_mcp_id: i64 = row.try_get("mcp_id").map_err(internal_error)?;
            if existing_mcp_id != mcp_id {
                return Err(error_response_coded(
                    StatusCode::CONFLICT,
                    format!(
                        "MCP binding alias '{}' already belongs to a different server",
                        request.binding.alias.trim()
                    ),
                    "mcp_binding_alias_conflict",
                ));
            }
            let binding_id: i64 = row.try_get("id").map_err(internal_error)?;
            query(
                "UPDATE mcp_bindings SET key_value_encrypted = ?, comment = ?, \
                 is_active = 1, updated_at = NOW(6) WHERE id = ? AND owner_user_id = ?",
            )
            .bind(&encrypted_key_value)
            .bind(request.binding.comment.as_deref())
            .bind(binding_id)
            .bind(&owner_user_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
            binding_id
        } else {
            query(
                "INSERT INTO mcp_bindings \
                 (owner_user_id, mcp_id, alias, key_value_encrypted, comment, is_active, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, 1, NOW(6), NOW(6))",
            )
            .bind(&owner_user_id)
            .bind(mcp_id)
            .bind(request.binding.alias.trim())
            .bind(&encrypted_key_value)
            .bind(request.binding.comment.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                if is_duplicate_key_error(&error) {
                    error_response_coded(
                        StatusCode::CONFLICT,
                        format!(
                            "MCP binding alias '{}' already exists",
                            request.binding.alias.trim()
                        ),
                        "mcp_binding_alias_conflict",
                    )
                } else {
                    internal_error(error)
                }
            })?;

            let row = query(
                "SELECT id FROM mcp_bindings \
                 WHERE owner_user_id = ? AND alias = ? AND is_active = 1",
            )
            .bind(&owner_user_id)
            .bind(request.binding.alias.trim())
            .fetch_one(&mut *tx)
            .await
            .map_err(internal_error)?;
            row.try_get("id").map_err(internal_error)?
        };

        query("DELETE FROM mcp_tools WHERE binding_id = ?")
            .bind(binding_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

        if !discovered_tools.is_empty() {
            let mut builder = QueryBuilder::<MySql>::new(
                "INSERT INTO mcp_tools \
                 (binding_id, tool_name, public_name, description, input_schema_json, \
                  output_schema_json, schema_hash, discovered_at) ",
            );
            builder.push_values(&discovered_tools, |mut row, tool| {
                let input_json = tool
                    .input_schema_json
                    .as_ref()
                    .map(serde_json::Value::to_string);
                let output_json = tool
                    .output_schema_json
                    .as_ref()
                    .map(serde_json::Value::to_string);
                row.push_bind(binding_id)
                    .push_bind(&tool.tool_name)
                    .push_bind(&tool.public_name)
                    .push_bind(tool.description.as_deref())
                    .push_bind(input_json)
                    .push_bind(output_json)
                    .push_bind(&tool.schema_hash)
                    .push("NOW(6)");
            });
            builder.build().execute(&mut *tx).await.map_err(|error| {
                if is_duplicate_key_error(&error) {
                    error_response_coded(
                        StatusCode::CONFLICT,
                        "duplicate MCP tool public name",
                        "mcp_public_name_conflict",
                    )
                } else {
                    internal_error(error)
                }
            })?;
        }

        tx.commit().await.map_err(internal_error)?;

        Ok(McpRegisterRecord {
            mcp_id,
            binding_id,
            server_name: request.server.name.trim().to_string(),
            binding_alias: request.binding.alias.trim().to_string(),
            tools: discovered_tools
                .into_iter()
                .map(|tool| McpRegisteredToolRecord {
                    tool_name: tool.tool_name,
                    public_name: tool.public_name,
                })
                .collect(),
        })
    }

    async fn load_runtime_bindings(
        &self,
        owner_user_id: String,
        binding_ids: &[i64],
    ) -> Result<Vec<McpRuntimeBindingRecord>, (StatusCode, Json<ErrorResponse>)> {
        if binding_ids.is_empty() {
            return Ok(Vec::new());
        }

        let unique_ids = canonical_binding_ids(binding_ids)?;

        let pool = self.get_pool().await.map_err(internal_error)?;
        let mut builder = QueryBuilder::<MySql>::new(
            "SELECT b.id AS binding_id, b.mcp_id AS mcp_id, b.alias AS binding_alias, \
             b.key_value_encrypted AS key_value_encrypted, \
             s.name AS server_name, s.description AS server_description, \
             s.transport AS transport, s.url AS url \
             FROM mcp_bindings b JOIN mcp_servers s ON b.mcp_id = s.id \
             WHERE b.owner_user_id = ",
        );
        builder.push_bind(&owner_user_id);
        builder.push(" AND b.is_active = 1 AND s.is_active = 1 AND b.id IN (");
        {
            let mut separated = builder.separated(", ");
            for id in &unique_ids {
                separated.push_bind(id);
            }
        }
        builder.push(") ORDER BY b.id ASC");

        let rows = builder
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        if rows.len() != unique_ids.len() {
            return Err(error_response_coded(
                StatusCode::NOT_FOUND,
                "one or more MCP bindings were not found",
                "mcp_binding_not_found",
            ));
        }

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let encrypted: String = row.try_get("key_value_encrypted").map_err(internal_error)?;
            records.push(McpRuntimeBindingRecord {
                binding_id: row.try_get("binding_id").map_err(internal_error)?,
                mcp_id: row.try_get("mcp_id").map_err(internal_error)?,
                server_name: row.try_get("server_name").map_err(internal_error)?,
                server_description: row.try_get("server_description").ok(),
                transport: row.try_get("transport").map_err(internal_error)?,
                url: row.try_get("url").map_err(internal_error)?,
                binding_alias: row.try_get("binding_alias").map_err(internal_error)?,
                key_value: self.decrypt_key_value(&encrypted)?,
                tools: Vec::new(),
            });
        }

        let mut tool_builder = QueryBuilder::<MySql>::new(
            "SELECT binding_id, tool_name, public_name, description, \
             CAST(input_schema_json AS CHAR) AS input_schema_json, \
             CAST(output_schema_json AS CHAR) AS output_schema_json, schema_hash \
             FROM mcp_tools WHERE binding_id IN (",
        );
        {
            let mut separated = tool_builder.separated(", ");
            for id in &unique_ids {
                separated.push_bind(id);
            }
        }
        tool_builder.push(") ORDER BY binding_id ASC, public_name ASC");
        let tool_rows = tool_builder
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        for row in tool_rows {
            let binding_id: i64 = row.try_get("binding_id").map_err(internal_error)?;
            if let Some(record) = records
                .iter_mut()
                .find(|record| record.binding_id == binding_id)
            {
                record.tools.push(McpDiscoveredToolData {
                    tool_name: row.try_get("tool_name").map_err(internal_error)?,
                    public_name: row.try_get("public_name").map_err(internal_error)?,
                    description: row.try_get("description").ok(),
                    input_schema_json: parse_json_column(row.try_get("input_schema_json").ok()),
                    output_schema_json: parse_json_column(row.try_get("output_schema_json").ok()),
                    schema_hash: row.try_get("schema_hash").map_err(internal_error)?,
                });
            }
        }

        records.sort_by_key(|record| record.binding_id);
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(
            FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=")
                .expect("test key must be valid"),
        )
    }

    #[test]
    fn runtime_record_debug_redacts_key_value() {
        let record = McpRuntimeBindingRecord {
            binding_id: 1,
            mcp_id: 2,
            server_name: "srv".to_string(),
            server_description: None,
            transport: "sse".to_string(),
            url: "http://example.test/mcp".to_string(),
            binding_alias: "alias".to_string(),
            key_value: serde_json::json!({"headers": {"Authorization": "Bearer secret-token"}}),
            tools: Vec::new(),
        };
        let debug = format!("{record:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret-token"));
    }

    #[tokio::test]
    async fn key_value_encrypt_decrypt_round_trips() {
        let service = DatabaseMcpRegistryService::new(
            MatrixOneSettings {
                host: "localhost".to_string(),
                port: 6001,
                user: "root".to_string(),
                password: "test".to_string(),
                database: "astra_test".to_string(),
            },
            encryptor(),
        );
        let key_value = serde_json::json!({
            "auth_token": "token-value",
            "headers": {"Authorization": "Bearer token-value"}
        });
        let encrypted = service.encrypted_key_value(&key_value).unwrap();
        assert!(!encrypted.contains("token-value"));
        let decrypted = service.decrypt_key_value(&encrypted).unwrap();
        assert_eq!(decrypted, key_value);
    }

    #[test]
    fn validate_upsert_rejects_public_name_collision() {
        let request = McpRegisterRequestData {
            server: McpServerRequestData {
                name: "srv".to_string(),
                description: None,
                transport: "sse".to_string(),
                url: "http://example.test/mcp".to_string(),
            },
            binding: McpBindingRequestData {
                alias: "alias".to_string(),
                key_value: serde_json::json!({}),
                comment: None,
            },
        };
        let tools = vec![
            McpDiscoveredToolData {
                tool_name: "a".to_string(),
                public_name: "mcp__alias__a".to_string(),
                description: None,
                input_schema_json: Some(serde_json::json!({"type": "object"})),
                output_schema_json: None,
                schema_hash: "h1".to_string(),
            },
            McpDiscoveredToolData {
                tool_name: "b".to_string(),
                public_name: "mcp__alias__a".to_string(),
                description: None,
                input_schema_json: Some(serde_json::json!({"type": "object"})),
                output_schema_json: None,
                schema_hash: "h2".to_string(),
            },
        ];
        let (status, Json(error)) = validate_upsert_input("u1", &request, &tools).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            error.error_code.as_deref(),
            Some("mcp_public_name_conflict")
        );
    }

    #[test]
    fn canonical_binding_ids_dedupes_and_sorts() {
        assert_eq!(
            canonical_binding_ids(&[301, 7, 301, 42]).unwrap(),
            vec![7, 42, 301]
        );
    }

    #[test]
    fn canonical_binding_ids_rejects_non_positive_ids() {
        let (status, Json(error)) = canonical_binding_ids(&[1, 0]).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error.error_code.as_deref(), Some("mcp_binding_invalid"));
    }
}
