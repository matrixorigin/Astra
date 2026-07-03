//! Server-visible skill catalog assembly.
//!
//! This module owns the visibility boundary for server-backed skill discovery.
//! A server-backed catalog is not the same thing as a standalone CLI catalog:
//!
//! - Server-local skills come only from the API server process HOME
//!   (`~/.astra/skills` and `~/.claude/skills`). They have no per-user ACL and
//!   are treated as deployment-level public skills for every authenticated user
//!   of that API server.
//! - Database skills come from `skills_registry` through `DatabaseSkillProvider`;
//!   that provider delegates visibility to `SkillService`, whose query contract
//!   is `created_by = current_user OR is_public = 1`.
//! - Project-local CLI skills (`{cwd}/.astra/skills`, `{cwd}/.claude/skills`,
//!   `{cwd}/skills`) are intentionally excluded here. They remain local to the
//!   CLI process unless the user publishes/registers them into the database.

use std::sync::Arc;

use astra_core::ErrorResponse;
use astra_services::{
    pagination::MAX_API_LIST_LIMIT,
    skills::{
        SkillListCursor, SkillListItem, SkillListRecord, SkillRecord, SkillService,
        skill_list_cursor_from_item,
    },
};
use axum::{Json, http::StatusCode};

use super::{
    DatabaseSkillProvider, LocalSkillProvider, SkillProvider as _, SkillSourceKind,
    UnifiedSkillRegistry,
};

const SERVER_CATALOG_DB_PAGE_SIZE: u32 = 500;
const SERVER_CATALOG_DB_MAX_ROWS: u32 = 5_000;

/// Build the full catalog visible to one authenticated user on this API server.
pub fn build_server_visible_skill_registry(
    skill_service: Option<Arc<dyn SkillService>>,
    user_id: &str,
) -> Option<Arc<UnifiedSkillRegistry>> {
    let mut registry = UnifiedSkillRegistry::new();
    registry.add_provider(Box::new(LocalSkillProvider::home_global()));

    if let Some(service) = skill_service {
        registry.add_provider(Box::new(DatabaseSkillProvider::new(
            service,
            user_id.to_string(),
        )));
    }

    let registry = Arc::new(registry);
    discover_registry_now(&registry);

    if registry.is_empty() {
        None
    } else {
        Some(registry)
    }
}

/// Discover providers on a synchronously built registry.
///
/// The server run-state builder is synchronous today. Keep the blocking bridge
/// centralized here so handler/runtime paths use the same discovery semantics
/// and future async refactors have one place to replace.
pub fn discover_registry_now(registry: &Arc<UnifiedSkillRegistry>) {
    fn log_discovery_result(result: Result<Vec<String>, astra_skills::traits::SkillError>) {
        if let Err(source) = result {
            tracing::warn!(error = %source, "skill catalog discovery failed");
        }
    }

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let registry = Arc::clone(registry);
        match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                log_discovery_result(tokio::task::block_in_place(|| {
                    handle.block_on(registry.discover_all())
                }));
            }
            _ => {
                let joined = std::thread::scope(|scope| {
                    scope
                        .spawn(|| handle.block_on(registry.discover_all()))
                        .join()
                });
                match joined {
                    Ok(result) => log_discovery_result(result),
                    Err(_) => tracing::warn!("skill catalog discovery thread panicked"),
                }
            }
        }
    }
}

/// Render a visible server catalog into the legacy `/skills` list response.
///
/// This keeps the Web UI picker and runtime turn resolver aligned: both read
/// the same server-visible catalog instead of one path seeing database rows and
/// the other path seeing filesystem skills.
pub async fn list_server_visible_skills(
    skill_service: Arc<dyn SkillService>,
    user_id: &str,
    limit: u32,
    cursor: Option<SkillListCursor>,
) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
    let limit = limit.clamp(1, MAX_API_LIST_LIMIT);

    let mut skills = LocalSkillProvider::home_global()
        .discover()
        .await
        .map_err(|source| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(format!(
                    "failed to discover api-server local skills: {source}"
                ))),
            )
        })?
        .into_iter()
        .filter(|manifest| manifest.user_invocable)
        .map(skill_list_item_from_manifest)
        .collect::<Vec<_>>();

    skills.extend(list_database_skill_items(skill_service, user_id).await?);
    skills = dedupe_skill_list_items(skills);

    skills.sort_by(|left, right| {
        left.skill_name
            .cmp(&right.skill_name)
            .then_with(|| left.version.cmp(&right.version))
            .then_with(|| left.skill_id.cmp(&right.skill_id))
    });

    let total = skills.len() as i64;
    let start = cursor
        .as_ref()
        .map(|cursor| {
            skills
                .iter()
                .position(|item| skill_item_after_cursor(item, cursor))
                .unwrap_or(skills.len())
        })
        .unwrap_or(0);
    let end = start.saturating_add(limit as usize + 1).min(skills.len());
    let mut page = skills[start..end].to_vec();
    let has_more = page.len() > limit as usize;
    if has_more {
        page.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        page.last().map(skill_list_cursor_from_item).transpose()?
    } else {
        None
    };

    Ok(SkillListRecord {
        skills: page,
        total: Some(total),
        limit,
        next_cursor,
    })
}

/// Load one skill from the same server-visible catalog used by `/skills`.
///
/// Local API-server skills are checked first to match the list/dedupe
/// precedence above. Database lookup remains delegated to `SkillService`, which
/// enforces `created_by = current_user OR is_public = 1`.
pub async fn get_server_visible_skill(
    skill_service: Arc<dyn SkillService>,
    user_id: String,
    skill_id: String,
    version: Option<String>,
) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
    if version.is_none() {
        match LocalSkillProvider::home_global().load(&skill_id).await {
            Ok(loaded) => return Ok(skill_record_from_loaded_skill(loaded)),
            Err(astra_skills::traits::SkillError::NotFound(_)) => {}
            Err(source) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new(format!(
                        "failed to load api-server local skill '{skill_id}': {source}"
                    ))),
                ));
            }
        }
    }

    skill_service.get_skill(user_id, skill_id, version).await
}

fn skill_source_label(source: &SkillSourceKind) -> &'static str {
    match source {
        SkillSourceKind::Local => "local",
        SkillSourceKind::Bundled => "bundled",
        SkillSourceKind::Database => "database",
        SkillSourceKind::Mcp => "mcp",
        SkillSourceKind::Plugin => "plugin",
    }
}

fn skill_list_item_from_manifest(manifest: astra_skills::manifest::SkillManifest) -> SkillListItem {
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
        source: Some(skill_source_label(&manifest.source).to_string()),
        category: manifest.category,
        created_at: None,
    }
}

fn skill_record_from_loaded_skill(loaded: astra_skills::manifest::LoadedSkill) -> SkillRecord {
    let manifest = loaded.manifest;
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "instructions".to_string(),
        serde_json::Value::String(loaded.instructions),
    );
    metadata.insert(
        "source".to_string(),
        serde_json::Value::String(skill_source_label(&manifest.source).to_string()),
    );
    metadata.insert(
        "execution_context".to_string(),
        serde_json::Value::String(
            match manifest.execution_context {
                astra_skills::manifest::ExecutionContext::Inline => "inline",
                astra_skills::manifest::ExecutionContext::Fork => "fork",
            }
            .to_string(),
        ),
    );
    metadata.insert(
        "user_invocable".to_string(),
        serde_json::Value::Bool(manifest.user_invocable),
    );
    if let Some(category) = manifest.category.clone() {
        metadata.insert("category".to_string(), serde_json::Value::String(category));
    }
    if !manifest.tags.is_empty() {
        metadata.insert("tags".to_string(), serde_json::json!(manifest.tags));
    }
    if !manifest.allowed_tools.is_empty() {
        metadata.insert(
            "allowed_tools".to_string(),
            serde_json::json!(manifest.allowed_tools),
        );
    }

    SkillRecord {
        skill_id: manifest.name.clone(),
        skill_name: manifest.name,
        version: manifest.version.to_string(),
        description: if manifest.description.is_empty() {
            None
        } else {
            Some(manifest.description)
        },
        metadata: Some(serde_json::Value::Object(metadata)),
        created_at: None,
    }
}

fn dedupe_skill_list_items(items: Vec<SkillListItem>) -> Vec<SkillListItem> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::with_capacity(items.len());
    for item in items {
        if seen.insert(item.skill_name.clone()) {
            deduped.push(item);
        }
    }
    deduped
}

async fn list_database_skill_items(
    skill_service: Arc<dyn SkillService>,
    user_id: &str,
) -> Result<Vec<SkillListItem>, (StatusCode, Json<ErrorResponse>)> {
    let mut cursor = None;
    let mut skills = Vec::new();
    loop {
        let remaining = SERVER_CATALOG_DB_MAX_ROWS.saturating_sub(skills.len() as u32);
        if remaining == 0 {
            break;
        }
        let limit = remaining.min(SERVER_CATALOG_DB_PAGE_SIZE);
        let page = skill_service
            .list_skills(user_id.to_string(), limit, cursor)
            .await?;
        let page_len = page.skills.len() as u32;
        cursor = page.next_cursor.clone();
        skills.extend(page.skills);
        if page_len < limit || cursor.is_none() {
            break;
        }
    }
    Ok(skills)
}

fn skill_item_after_cursor(item: &SkillListItem, cursor: &SkillListCursor) -> bool {
    (
        item.skill_name.as_str(),
        item.version.as_str(),
        item.skill_id.as_str(),
    ) > (
        cursor.skill_name.as_str(),
        cursor.version.as_str(),
        cursor.skill_id.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillResolver as _;
    use astra_skills::providers::LocalSkillProvider;

    #[tokio::test]
    async fn server_visible_registry_can_be_built_without_database_service() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let skill_dir = temp
            .path()
            .join(".astra")
            .join("skills")
            .join("server-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: server-skill\ndescription: Server local skill\n---\nUse me.\n",
        )
        .expect("skill file");

        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(LocalSkillProvider::with_paths(vec![
            temp.path().join(".astra").join("skills"),
        ])));
        let registry = Arc::new(registry);
        registry.discover_all().await.expect("discover");

        let resolver = super::super::UnifiedSkillResolver::new(registry);
        let names = resolver
            .available_skills()
            .into_iter()
            .map(|skill| skill.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["server-skill".to_string()],
            "server-local HOME skills must be invocable through the shared resolver"
        );
    }
}
