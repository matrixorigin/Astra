//! Database skill provider — adapts `SkillService` to the `SkillProvider` trait.
//!
//! Wraps the existing `astra_services::skills::SkillService` to expose
//! database-backed skills through the unified skill framework.

use async_trait::async_trait;
use std::sync::Arc;

use astra_services::skills::SkillService;

use crate::skills::manifest::{ExecutionContext, LoadedSkill, SkillManifest, SkillSourceKind};
use crate::skills::traits::{SkillError, SkillProvider};

/// Adapts `SkillService` (database-backed) to the `SkillProvider` trait.
///
/// Skills in the database follow a different schema from SKILL.md files,
/// so this adapter maps between the two representations.
pub struct DatabaseSkillProvider {
    service: Arc<dyn SkillService>,
}

impl DatabaseSkillProvider {
    pub fn new(service: Arc<dyn SkillService>) -> Self {
        Self { service }
    }

    fn get_str<'a>(
        obj: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<&'a str> {
        obj.get(key).and_then(serde_json::Value::as_str)
    }

    fn get_string_vec(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Vec<String> {
        obj.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn parse_execution_context(
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> ExecutionContext {
        match Self::get_str(obj, "execution_context")
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "fork" => ExecutionContext::Fork,
            _ => ExecutionContext::Inline,
        }
    }

    fn parse_trust_tier(
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> crate::skills::manifest::TrustTier {
        match Self::get_str(obj, "trust_tier")
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "bundled" => crate::skills::manifest::TrustTier::Bundled,
            "verified" => crate::skills::manifest::TrustTier::Verified,
            "community" => crate::skills::manifest::TrustTier::Community,
            _ => crate::skills::manifest::TrustTier::Unverified,
        }
    }
}

#[async_trait]
impl SkillProvider for DatabaseSkillProvider {
    fn source_kind(&self) -> SkillSourceKind {
        SkillSourceKind::Database
    }

    async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
        let result = self.service.list_skills(200, 0).await.map_err(|(_, err)| {
            SkillError::Internal(format!(
                "failed to list skills from database: {}",
                err.0.detail
            ))
        })?;

        let manifests = result
            .skills
            .into_iter()
            .map(|item| {
                let version = item.version.parse().unwrap_or_default();

                SkillManifest {
                    name: item.skill_name,
                    version,
                    description: item.description.unwrap_or_default(),
                    source: SkillSourceKind::Database,
                    category: item.category,
                    ..Default::default()
                }
            })
            .collect();

        Ok(manifests)
    }

    async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        let record = self
            .service
            .get_skill(name.to_string(), None)
            .await
            .map_err(|(status, err)| {
                if status == axum::http::StatusCode::NOT_FOUND {
                    SkillError::NotFound(format!("database skill not found: {name}"))
                } else {
                    SkillError::LoadFailed(format!(
                        "failed to load skill '{}' from database: {}",
                        name, err.0.detail
                    ))
                }
            })?;

        let version = record.version.parse().unwrap_or_default();
        let metadata = record.metadata.unwrap_or_else(|| serde_json::json!({}));
        let metadata_obj = metadata.as_object().cloned().unwrap_or_default();
        let instructions = Self::get_str(&metadata_obj, "instructions")
            .unwrap_or("")
            .to_string();
        let remote_url = Self::get_str(&metadata_obj, "remote_url").map(str::to_string);
        let input_schema = metadata_obj.get("input_schema").cloned();
        let output_schema = metadata_obj.get("output_schema").cloned();
        let forward_headers = Self::get_string_vec(&metadata_obj, "forward_headers");
        let required_headers = Self::get_string_vec(&metadata_obj, "required_headers");
        let user_invocable = metadata_obj
            .get("user_invocable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        let instruction_tokens = (instructions.len() as u32) / 4;

        let manifest = SkillManifest {
            name: record.skill_name,
            version,
            description: record.description.unwrap_or_default(),
            source: SkillSourceKind::Database,
            execution_context: Self::parse_execution_context(&metadata_obj),
            user_invocable,
            triggers: Self::get_string_vec(&metadata_obj, "triggers"),
            when_to_use: Self::get_str(&metadata_obj, "when_to_use").map(str::to_string),
            category: Self::get_str(&metadata_obj, "category").map(str::to_string),
            tags: Self::get_string_vec(&metadata_obj, "tags"),
            input_schema,
            output_schema,
            remote_url,
            forward_headers,
            required_headers,
            aliases: Self::get_string_vec(&metadata_obj, "aliases"),
            trust_tier: Self::parse_trust_tier(&metadata_obj),
            ..Default::default()
        };

        Ok(LoadedSkill {
            manifest,
            instructions,
            instruction_tokens,
            resources: None,
            skill_dir: None,
        })
    }

    async fn refresh(&self) -> Result<(), SkillError> {
        // Database is always "fresh" — each discover/load hits the DB directly.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::ErrorResponse;
    use astra_services::skills::*;
    use axum::Json;
    use axum::http::StatusCode;

    struct MockSkillService {
        skills: Vec<SkillListItem>,
    }

    #[async_trait]
    impl SkillService for MockSkillService {
        async fn register_skill(
            &self,
            _: String,
            _: SkillRegisterRequestData,
        ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!()
        }

        async fn list_skills(
            &self,
            limit: u32,
            offset: u32,
        ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
            let end = ((offset + limit) as usize).min(self.skills.len());
            let start = (offset as usize).min(end);
            Ok(SkillListRecord {
                skills: self.skills[start..end].to_vec(),
                total: self.skills.len() as i64,
                limit,
                offset,
            })
        }

        async fn get_skill(
            &self,
            skill_id: String,
            _version: Option<String>,
        ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
            self.skills
                .iter()
                .find(|s| s.skill_name == skill_id || s.skill_id == skill_id)
                .map(|s| {
                    let metadata = if s.skill_name == "remote-http" {
                        serde_json::json!({
                            "skill_type": "remote",
                            "remote_url": "http://127.0.0.1:18080/skills/execute",
                            "input_schema": {
                                "type": "object",
                                "properties": {
                                    "task": {"type": "string"}
                                }
                            },
                            "output_schema": {
                                "type": "object",
                                "properties": {
                                    "result": {"type": "string"}
                                }
                            },
                            "forward_headers": [
                                "authorization",
                                "x-workspace-id"
                            ],
                            "required_headers": [
                                "x-workspace-id"
                            ]
                        })
                    } else {
                        serde_json::json!({"instructions": "DB skill instructions."})
                    };
                    SkillRecord {
                        skill_id: s.skill_id.clone(),
                        skill_name: s.skill_name.clone(),
                        version: s.version.clone(),
                        description: s.description.clone(),
                        metadata: Some(metadata),
                        created_at: s.created_at.clone(),
                    }
                })
                .ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        Json(ErrorResponse::new(format!(
                            "Skill '{}' not found",
                            skill_id
                        ))),
                    )
                })
        }

        async fn get_skill_info(
            &self,
            _: String,
            _: String,
        ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!()
        }

        async fn list_skill_versions(
            &self,
            _: String,
        ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!()
        }

        async fn get_skill_status(
            &self,
            _: String,
            _: u32,
        ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!()
        }

        async fn publish_skill(
            &self,
            _: String,
            _: SkillPublishRequestData,
        ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!()
        }

        async fn unpublish_skill(
            &self,
            _: String,
            _: String,
        ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!()
        }
    }

    fn mock_service() -> Arc<dyn SkillService> {
        Arc::new(MockSkillService {
            skills: vec![
                SkillListItem {
                    skill_id: "review@1.0.0".into(),
                    skill_name: "review".into(),
                    version: "1.0.0".into(),
                    description: Some("Code review skill".into()),
                    status: Some("active".into()),
                    source: Some("user".into()),
                    category: Some("code-quality".into()),
                    created_at: None,
                },
                SkillListItem {
                    skill_id: "deploy@2.0.0".into(),
                    skill_name: "deploy".into(),
                    version: "2.0.0".into(),
                    description: Some("Deployment automation".into()),
                    status: Some("active".into()),
                    source: Some("marketplace".into()),
                    category: Some("devops".into()),
                    created_at: None,
                },
                SkillListItem {
                    skill_id: "remote-http@1.0.0".into(),
                    skill_name: "remote-http".into(),
                    version: "1.0.0".into(),
                    description: Some("Remote HTTP skill".into()),
                    status: Some("active".into()),
                    source: Some("user".into()),
                    category: Some("integration".into()),
                    created_at: None,
                },
            ],
        })
    }

    #[tokio::test]
    async fn discover_lists_all_skills() {
        let provider = DatabaseSkillProvider::new(mock_service());
        let manifests = provider.discover().await.unwrap();
        assert_eq!(manifests.len(), 3);

        let names: Vec<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"review"));
        assert!(names.contains(&"deploy"));
        assert!(names.contains(&"remote-http"));

        assert!(
            manifests
                .iter()
                .all(|m| m.source == SkillSourceKind::Database)
        );
    }

    #[tokio::test]
    async fn load_returns_instructions() {
        let provider = DatabaseSkillProvider::new(mock_service());
        let loaded = provider.load("review").await.unwrap();
        assert_eq!(loaded.manifest.name, "review");
        assert_eq!(loaded.instructions, "DB skill instructions.");
        assert_eq!(loaded.manifest.source, SkillSourceKind::Database);
    }

    #[tokio::test]
    async fn load_not_found() {
        let provider = DatabaseSkillProvider::new(mock_service());
        let result = provider.load("nonexistent").await;
        assert!(matches!(result, Err(SkillError::NotFound(_))));
    }

    #[tokio::test]
    async fn load_remote_skill_maps_remote_url_and_schema() {
        let provider = DatabaseSkillProvider::new(mock_service());
        let loaded = provider.load("remote-http").await.unwrap();
        assert_eq!(loaded.manifest.name, "remote-http");
        assert_eq!(
            loaded.manifest.remote_url.as_deref(),
            Some("http://127.0.0.1:18080/skills/execute")
        );
        assert!(loaded.manifest.input_schema.is_some());
        assert!(loaded.manifest.output_schema.is_some());
        assert_eq!(
            loaded.manifest.forward_headers,
            vec!["authorization".to_string(), "x-workspace-id".to_string()]
        );
        assert_eq!(
            loaded.manifest.required_headers,
            vec!["x-workspace-id".to_string()]
        );
    }
}
