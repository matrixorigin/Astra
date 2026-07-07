use astra_runtime::skills::UnifiedSkillRegistry;
use astra_services::skills::{SkillListCursor, SkillListItem, SkillListRecord, SkillRecord};

const SUPPORTED_SKILL_SOURCE_FILTERS: &[&str] = astra_skills::SkillSourceKind::SUPPORTED_FILTERS;

/// Hard upper bound on a single `skill list` page. The registry loads every
/// matching manifest into memory before slicing, so an attacker (or a careless
/// `--limit 4_294_967_295`) can no longer drag the CLI process into a multi-GB
/// allocation by setting an absurdly large limit. CLI listing is an explicit
/// operator action rather than prompt surfacing, but still bounded tightly
/// enough to avoid pathological memory use from absurd page sizes.
pub(crate) const SKILL_CATALOG_MAX_LIMIT: u32 = 500;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SkillCatalogFilter {
    pub query: Option<String>,
    pub source: Option<String>,
    pub category: Option<String>,
}

pub(crate) fn source_label(source: &astra_skills::SkillSourceKind) -> &'static str {
    source.as_str()
}

pub(crate) fn normalize_source_filter(raw: &str) -> Result<String, String> {
    let normalized = raw.trim().to_lowercase();
    if SUPPORTED_SKILL_SOURCE_FILTERS.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(format!(
            "unsupported skill source '{raw}'; expected one of: {}",
            SUPPORTED_SKILL_SOURCE_FILTERS.join(", ")
        ))
    }
}

impl SkillCatalogFilter {
    pub(crate) fn matches(&self, manifest: &astra_skills::SkillManifest) -> bool {
        if let Some(source) = self.source.as_deref() {
            if source_label(&manifest.source) != source {
                return false;
            }
        }

        if let Some(category) = self.category.as_deref() {
            match manifest.category.as_deref() {
                Some(candidate) if candidate.to_lowercase() == category => {}
                _ => return false,
            }
        }

        if let Some(query) = self.query.as_deref() {
            let tokens = query
                .split_whitespace()
                .filter(|token| !token.is_empty())
                .collect::<Vec<_>>();
            let name = manifest.name.to_lowercase();
            let description = manifest.description.to_lowercase();
            let tags = manifest
                .tags
                .iter()
                .map(|tag| tag.to_lowercase())
                .collect::<Vec<_>>();
            let category = manifest
                .category
                .as_ref()
                .map(|category| category.to_lowercase());
            if !tokens.iter().all(|token| {
                name.contains(token)
                    || description.contains(token)
                    || tags.iter().any(|tag| tag.contains(token))
                    || category
                        .as_ref()
                        .is_some_and(|category| category.contains(token))
            }) {
                return false;
            }
        }

        true
    }
}

pub(crate) fn list_skill_record_from_registry(
    registry: &UnifiedSkillRegistry,
    filter: &SkillCatalogFilter,
    limit: u32,
    offset: u32,
) -> SkillListRecord {
    let effective_limit = limit.min(SKILL_CATALOG_MAX_LIMIT);

    let mut manifests: Vec<_> = registry
        .all_manifests()
        .into_iter()
        .filter(|manifest| manifest.user_invocable)
        .filter(|manifest| filter.matches(manifest))
        .collect();

    manifests.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.version.cmp(&right.version))
    });

    let total = manifests.len() as i64;
    let start = (offset as usize).min(manifests.len());
    let end = start
        .saturating_add(effective_limit as usize)
        .min(manifests.len());

    let skills = manifests[start..end]
        .iter()
        .cloned()
        .map(skill_list_item_from_manifest)
        .collect::<Vec<_>>();
    let next_cursor = if end < manifests.len() {
        skills.last().map(|item| SkillListCursor {
            skill_name: item.skill_name.clone(),
            version: item.version.clone(),
            skill_id: item.skill_id.clone(),
        })
    } else {
        None
    };

    SkillListRecord {
        skills,
        total: Some(total),
        // Echo the *applied* cap, not the user's request, so cursor callers
        // can reason about whether a short page means end-of-list.
        limit: effective_limit,
        next_cursor,
    }
}

pub(crate) async fn load_skill_record_from_registry(
    registry: &UnifiedSkillRegistry,
    skill_id: &str,
    version: Option<&str>,
) -> Result<SkillRecord, String> {
    let loaded = registry
        .load(skill_id)
        .await
        .map_err(|source| format!("failed to load skill '{skill_id}': {source}"))?;
    if let Some(expected_version) = version {
        let actual_version = loaded.manifest.version.to_string();
        if actual_version != expected_version {
            return Err(format!(
                "skill '{skill_id}' resolved to version {actual_version}, not requested version {expected_version}"
            ));
        }
    }
    Ok(skill_record_from_loaded_skill(loaded))
}

fn skill_list_item_from_manifest(manifest: astra_skills::SkillManifest) -> SkillListItem {
    SkillListItem {
        skill_id: manifest.name.clone(),
        skill_name: manifest.name,
        version: manifest.version.to_string(),
        description: if manifest.description.is_empty() {
            None
        } else {
            Some(manifest.description)
        },
        status: Some("active".to_string()),
        source: Some(source_label(&manifest.source).to_string()),
        category: manifest.category,
        created_at: None,
    }
}

fn skill_record_from_loaded_skill(loaded: astra_skills::LoadedSkill) -> SkillRecord {
    let manifest = loaded.manifest;
    let metadata = SkillMetadata {
        instructions: loaded.instructions,
        source: source_label(&manifest.source).to_string(),
        execution_context: match manifest.execution_context {
            astra_skills::ExecutionContext::Inline => "inline",
            astra_skills::ExecutionContext::Fork => "fork",
        },
        user_invocable: manifest.user_invocable,
        category: manifest.category.clone(),
        tags: manifest.tags.clone(),
        allowed_tools: manifest.allowed_tools.clone(),
    };

    SkillRecord {
        skill_id: manifest.name.clone(),
        skill_name: manifest.name,
        version: manifest.version.to_string(),
        description: if manifest.description.is_empty() {
            None
        } else {
            Some(manifest.description)
        },
        metadata: Some(
            serde_json::to_value(metadata)
                .expect("SkillMetadata serialization should not fail for plain data fields"),
        ),
        created_at: None,
    }
}

#[derive(serde::Serialize)]
struct SkillMetadata {
    instructions: String,
    source: String,
    execution_context: &'static str,
    user_invocable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    allowed_tools: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        SKILL_CATALOG_MAX_LIMIT, SkillCatalogFilter, list_skill_record_from_registry,
        load_skill_record_from_registry, normalize_source_filter, source_label,
    };

    fn manifest(
        name: &str,
        source: astra_skills::SkillSourceKind,
        user_invocable: bool,
    ) -> astra_skills::SkillManifest {
        astra_skills::SkillManifest {
            name: name.to_string(),
            description: format!("{name} description"),
            category: Some("code-review".to_string()),
            source,
            user_invocable,
            ..Default::default()
        }
    }

    async fn registry_with_manifests(
        manifests: Vec<astra_skills::SkillManifest>,
    ) -> astra_runtime::skills::UnifiedSkillRegistry {
        use astra_runtime::skills::SkillProvider;
        use async_trait::async_trait;

        struct StubProvider {
            manifests: Vec<astra_skills::SkillManifest>,
        }

        #[async_trait]
        impl SkillProvider for StubProvider {
            fn source_kind(&self) -> astra_skills::SkillSourceKind {
                astra_skills::SkillSourceKind::Local
            }

            async fn discover(
                &self,
            ) -> Result<Vec<astra_skills::SkillManifest>, astra_skills::SkillError> {
                Ok(self.manifests.clone())
            }

            async fn load(
                &self,
                name: &str,
            ) -> Result<astra_skills::LoadedSkill, astra_skills::SkillError> {
                let manifest = self
                    .manifests
                    .iter()
                    .find(|manifest| manifest.name == name)
                    .cloned()
                    .ok_or_else(|| astra_skills::SkillError::NotFound(name.to_string()))?;
                Ok(astra_skills::LoadedSkill {
                    manifest,
                    instructions: "Instructions".to_string(),
                    instruction_tokens: 1,
                    resources: None,
                    skill_dir: None,
                })
            }

            async fn refresh(&self) -> Result<(), astra_skills::SkillError> {
                Ok(())
            }
        }

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider { manifests }));
        registry.discover_all().await.expect("discover");
        registry
    }

    #[tokio::test]
    async fn source_label_covers_database_and_plugin_sources() {
        assert_eq!(
            source_label(&astra_skills::SkillSourceKind::Database),
            "database"
        );
        assert_eq!(
            source_label(&astra_skills::SkillSourceKind::Plugin),
            "plugin"
        );
    }

    #[tokio::test]
    async fn list_skill_record_filters_user_invocable_and_database_source() {
        let registry = registry_with_manifests(vec![
            manifest("review", astra_skills::SkillSourceKind::Local, true),
            manifest("server-only", astra_skills::SkillSourceKind::Database, true),
            manifest("hidden", astra_skills::SkillSourceKind::Local, false),
        ])
        .await;
        let filter = SkillCatalogFilter {
            source: Some("database".to_string()),
            ..Default::default()
        };

        let record = list_skill_record_from_registry(&registry, &filter, 50, 0);

        assert_eq!(record.total, Some(1));
        assert_eq!(record.skills.len(), 1);
        assert_eq!(record.skills[0].skill_name, "server-only");
        assert_eq!(record.skills[0].source.as_deref(), Some("database"));
    }

    #[tokio::test]
    async fn list_skill_record_query_uses_and_semantics() {
        let mut review = manifest("review-changes", astra_skills::SkillSourceKind::Local, true);
        review.description = "review code changes".to_string();
        review.tags = vec!["review".to_string(), "changes".to_string()];
        let registry = registry_with_manifests(vec![
            review,
            manifest("review-only", astra_skills::SkillSourceKind::Local, true),
        ])
        .await;
        let filter = SkillCatalogFilter {
            query: Some("review changes".to_string()),
            ..Default::default()
        };

        let record = list_skill_record_from_registry(&registry, &filter, 50, 0);

        assert_eq!(record.total, Some(1));
        assert_eq!(record.skills[0].skill_name, "review-changes");
    }

    #[tokio::test]
    async fn load_skill_record_preserves_source_metadata() {
        let registry = registry_with_manifests(vec![manifest(
            "review",
            astra_skills::SkillSourceKind::Bundled,
            true,
        )])
        .await;

        let record = load_skill_record_from_registry(&registry, "review", None)
            .await
            .expect("load");

        let metadata = record.metadata.expect("metadata");
        assert_eq!(
            metadata.get("source").and_then(|v| v.as_str()),
            Some("bundled")
        );
    }

    #[tokio::test]
    async fn list_skill_record_caps_oversized_limit_at_max() {
        // 1000 manifests, limit=u32::MAX → must clamp to SKILL_CATALOG_MAX_LIMIT.
        let mut manifests = Vec::with_capacity(1000);
        for i in 0..1000 {
            manifests.push(manifest(
                &format!("s-{i:04}"),
                astra_skills::SkillSourceKind::Local,
                true,
            ));
        }
        let registry = registry_with_manifests(manifests).await;
        let filter = SkillCatalogFilter::default();

        let record = list_skill_record_from_registry(&registry, &filter, u32::MAX, 0);

        assert_eq!(
            record.skills.len(),
            SKILL_CATALOG_MAX_LIMIT as usize,
            "page size must be capped at SKILL_CATALOG_MAX_LIMIT"
        );
        assert_eq!(
            record.limit, SKILL_CATALOG_MAX_LIMIT,
            "echo the applied limit so cursor callers can trust short pages"
        );
        assert_eq!(
            record.total,
            Some(1000),
            "total must reflect every match, not the cap"
        );
    }

    #[tokio::test]
    async fn normalize_source_filter_accepts_database() {
        assert_eq!(
            normalize_source_filter("DATABASE").expect("database"),
            "database"
        );
    }

    #[tokio::test]
    async fn normalize_source_filter_rejects_dynamic() {
        let err = normalize_source_filter("dynamic").expect_err("dynamic should fail");
        assert!(err.contains("unsupported skill source"));
    }
}
