//! Database skill provider — adapts `SkillService` to the `SkillProvider` trait.
//!
//! Wraps the existing `astra_services::skills::SkillService` to expose
//! database-backed skills through the unified skill framework.

use async_trait::async_trait;
use std::sync::Arc;

use astra_services::skills::SkillService;

use crate::skills::manifest::{LoadedSkill, SkillManifest, SkillSourceKind};
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

        let instructions = record
            .metadata
            .as_ref()
            .and_then(|m| m.get("instructions"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let instruction_tokens = (instructions.len() as u32) / 4;

        let manifest = SkillManifest {
            name: record.skill_name,
            version,
            description: record.description.unwrap_or_default(),
            source: SkillSourceKind::Database,
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
                .map(|s| SkillRecord {
                    skill_id: s.skill_id.clone(),
                    skill_name: s.skill_name.clone(),
                    version: s.version.clone(),
                    description: s.description.clone(),
                    metadata: Some(serde_json::json!({"instructions": "DB skill instructions."})),
                    created_at: s.created_at.clone(),
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
            ],
        })
    }

    #[tokio::test]
    async fn discover_lists_all_skills() {
        let provider = DatabaseSkillProvider::new(mock_service());
        let manifests = provider.discover().await.unwrap();
        assert_eq!(manifests.len(), 2);

        let names: Vec<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"review"));
        assert!(names.contains(&"deploy"));

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
}
