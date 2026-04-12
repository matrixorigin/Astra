//! Marketplace quality signals — aggregated cross-user quality metrics and ranked search.
//!
//! Phase 3 of skill capability evolution: connects local SkillQualityTracker data
//! to a shared MatrixOne database for cross-user quality aggregation.

use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

use crate::pagination::clamp_marketplace_search_offset;

// ── Data types ───────────────────────────────────────────────────────────────

/// Anonymous quality report submitted by a user.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QualityReportData {
    pub skill_name: String,
    pub skill_version: String,
    pub runtime_version: String,
    pub success_rate: f64,
    pub avg_tokens: f64,
    pub invocation_count: u32,
}

/// Aggregated marketplace stats for a single skill.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillMarketplaceStats {
    pub skill_name: String,
    pub publisher_id: Option<String>,
    pub total_installs: i64,
    pub active_users_7d: i32,
    pub avg_quality: f64,
    pub avg_rating: f64,
    pub report_count: i32,
    pub compatibility_score: f64,
    pub trust_tier: Option<String>,
    pub last_updated: Option<String>,
}

/// A single result from ranked marketplace search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillSearchResult {
    pub skill_name: String,
    pub version: String,
    pub description: Option<String>,
    pub publisher_id: Option<String>,
    pub trust_tier: Option<String>,
    pub category: Option<String>,
    /// Composite ranking score (0.0–1.0).
    pub ranking_score: f64,
    pub avg_quality: f64,
    pub total_installs: i64,
    pub active_users_7d: i32,
}

/// Query parameters for ranked skill search.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkillSearchQuery {
    /// Free-text query matched against name + description.
    pub query: Option<String>,
    /// Filter by category.
    pub category: Option<String>,
    /// Filter by trust tier.
    pub trust_tier: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Response for search results.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillSearchResponse {
    pub results: Vec<SkillSearchResult>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

const MAX_SEARCH_RESULTS: u32 = 100;
const DEFAULT_SEARCH_LIMIT: u32 = 20;

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait MarketplaceStatsService: Send + Sync {
    /// Submit an anonymous quality report.
    async fn submit_quality_report(
        &self,
        report: QualityReportData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;

    /// Get aggregated stats for a specific skill.
    async fn get_skill_stats(
        &self,
        skill_name: String,
    ) -> Result<SkillMarketplaceStats, (StatusCode, Json<ErrorResponse>)>;

    /// Search skills with marketplace ranking.
    async fn search_ranked(
        &self,
        query: SkillSearchQuery,
    ) -> Result<SkillSearchResponse, (StatusCode, Json<ErrorResponse>)>;

    /// Refresh aggregated stats from individual quality reports.
    /// Typically called periodically or after batch report submission.
    async fn refresh_aggregation(
        &self,
        skill_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseMarketplaceStatsService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseMarketplaceStatsService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[async_trait]
impl MarketplaceStatsService for DatabaseMarketplaceStatsService {
    async fn submit_quality_report(
        &self,
        report: QualityReportData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        query(
            "INSERT INTO skill_quality_reports \
             (skill_name, skill_version, runtime_version, success_rate, avg_tokens, invocation_count) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&report.skill_name)
        .bind(&report.skill_version)
        .bind(&report.runtime_version)
        .bind(report.success_rate)
        .bind(report.avg_tokens)
        .bind(report.invocation_count)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        // Trigger aggregation refresh for this skill.
        self.refresh_aggregation(report.skill_name).await?;

        Ok(())
    }

    async fn get_skill_stats(
        &self,
        skill_name: String,
    ) -> Result<SkillMarketplaceStats, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let row = query(
            "SELECT skill_name, publisher_id, total_installs, active_users_7d, \
             avg_quality, avg_rating, report_count, compatibility_score, \
             trust_tier, last_updated \
             FROM skill_marketplace_stats WHERE skill_name = ?",
        )
        .bind(&skill_name)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("No marketplace stats for skill '{skill_name}'"),
            )
        })?;

        Ok(SkillMarketplaceStats {
            skill_name: row.try_get("skill_name").unwrap_or_default(),
            publisher_id: row.try_get("publisher_id").ok(),
            total_installs: row.try_get("total_installs").unwrap_or(0),
            active_users_7d: row.try_get("active_users_7d").unwrap_or(0),
            avg_quality: row.try_get("avg_quality").unwrap_or(0.0),
            avg_rating: row.try_get("avg_rating").unwrap_or(0.0),
            report_count: row.try_get("report_count").unwrap_or(0),
            compatibility_score: row.try_get("compatibility_score").unwrap_or(0.0),
            trust_tier: row.try_get("trust_tier").ok(),
            last_updated: row.try_get("last_updated").ok(),
        })
    }

    async fn search_ranked(
        &self,
        search: SkillSearchQuery,
    ) -> Result<SkillSearchResponse, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let limit = search
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .min(MAX_SEARCH_RESULTS);
        let offset = clamp_marketplace_search_offset(search.offset.unwrap_or(0));

        // Build dynamic WHERE clauses
        let mut conditions = Vec::new();
        let mut binds: Vec<String> = Vec::new();

        if let Some(ref q) = search.query {
            conditions.push("(sr.skill_name LIKE ? OR sr.description LIKE ?)".to_string());
            let like = format!("%{q}%");
            binds.push(like.clone());
            binds.push(like);
        }
        if let Some(ref cat) = search.category {
            conditions.push("sr.category = ?".to_string());
            binds.push(cat.clone());
        }
        if let Some(ref tier) = search.trust_tier {
            conditions.push("ms.trust_tier = ?".to_string());
            binds.push(tier.clone());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        // Ranking formula (computed in SQL):
        // score = 0.35 * quality + 0.25 * popularity + 0.20 * freshness + 0.15 * trust + 0.05 * compat
        //
        // - quality: COALESCE(ms.avg_quality, 0.5) — normalized [0,1]
        // - popularity: cap active_users at 1.0 — use CASE (MatrixOne has no LEAST())
        // - freshness: simplified to: CASE WHEN sr.updated_at IS NOT NULL THEN 0.5 ELSE 0.3 END
        // - trust: CASE trust_tier mapping
        // - compat: COALESCE(ms.compatibility_score, 0.5)
        let ranking_sql = "\
            0.35 * COALESCE(ms.avg_quality, 0.5) \
            + 0.25 * CASE \
                WHEN (COALESCE(ms.active_users_7d, 0) / 1000.0) < 1.0 \
                THEN (COALESCE(ms.active_users_7d, 0) / 1000.0) \
                ELSE 1.0 END \
            + 0.20 * CASE WHEN sr.updated_at IS NOT NULL THEN 0.5 ELSE 0.3 END \
            + 0.15 * CASE ms.trust_tier \
                WHEN 'bundled' THEN 1.0 \
                WHEN 'verified' THEN 0.8 \
                WHEN 'community' THEN 0.5 \
                ELSE 0.2 END \
            + 0.05 * COALESCE(ms.compatibility_score, 0.5)";

        let sql = format!(
            "SELECT sr.skill_name, sr.version, sr.description, \
             ms.publisher_id, ms.trust_tier, sr.category, \
             ({ranking_sql}) AS ranking_score, \
             COALESCE(ms.avg_quality, 0.0) AS avg_quality, \
             COALESCE(ms.total_installs, 0) AS total_installs, \
             COALESCE(ms.active_users_7d, 0) AS active_users_7d \
             FROM skills_registry sr \
             LEFT JOIN skill_marketplace_stats ms ON sr.skill_name = ms.skill_name \
             {where_clause} \
             ORDER BY ranking_score DESC \
             LIMIT ? OFFSET ?"
        );

        // Count total
        let count_sql = format!(
            "SELECT COUNT(*) AS cnt FROM skills_registry sr \
             LEFT JOIN skill_marketplace_stats ms ON sr.skill_name = ms.skill_name \
             {where_clause}"
        );

        // Build and execute count query
        let mut count_q = query(&count_sql);
        for b in &binds {
            count_q = count_q.bind(b);
        }
        let count_row = count_q.fetch_one(&pool).await.map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").unwrap_or(0);

        // Build and execute search query
        let mut search_q = query(&sql);
        for b in &binds {
            search_q = search_q.bind(b);
        }
        search_q = search_q.bind(limit).bind(offset);

        let rows = search_q.fetch_all(&pool).await.map_err(internal_error)?;

        let results = rows
            .iter()
            .map(|row| SkillSearchResult {
                skill_name: row.try_get("skill_name").unwrap_or_default(),
                version: row.try_get("version").unwrap_or_default(),
                description: row.try_get("description").ok(),
                publisher_id: row.try_get("publisher_id").ok(),
                trust_tier: row.try_get("trust_tier").ok(),
                category: row.try_get("category").ok(),
                ranking_score: row.try_get("ranking_score").unwrap_or(0.0),
                avg_quality: row.try_get("avg_quality").unwrap_or(0.0),
                total_installs: row.try_get("total_installs").unwrap_or(0),
                active_users_7d: row.try_get("active_users_7d").unwrap_or(0),
            })
            .collect();

        Ok(SkillSearchResponse {
            results,
            total,
            limit,
            offset,
        })
    }

    async fn refresh_aggregation(
        &self,
        skill_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        // Aggregate from quality reports
        let agg_row = query(
            "SELECT \
             AVG(success_rate) AS avg_quality, \
             COUNT(*) AS report_count, \
             AVG(avg_tokens) AS avg_tokens \
             FROM skill_quality_reports WHERE skill_name = ?",
        )
        .bind(&skill_name)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let avg_quality: f64 = agg_row.try_get("avg_quality").unwrap_or(0.0);
        let report_count: i64 = agg_row.try_get("report_count").unwrap_or(0);

        // Upsert into marketplace_stats
        // MatrixOne supports INSERT ... ON DUPLICATE KEY UPDATE
        query(
            "INSERT INTO skill_marketplace_stats (skill_name, avg_quality, report_count, last_updated) \
             VALUES (?, ?, ?, NOW()) \
             ON DUPLICATE KEY UPDATE \
             avg_quality = VALUES(avg_quality), \
             report_count = VALUES(report_count), \
             last_updated = NOW()",
        )
        .bind(&skill_name)
        .bind(avg_quality)
        .bind(report_count as i32)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(())
    }
}

// ── Noop implementation (for tests / offline mode) ───────────────────────────

/// No-op implementation for when the database is unavailable.
#[derive(Clone, Debug, Default)]
pub struct NoopMarketplaceStatsService;

#[async_trait]
impl MarketplaceStatsService for NoopMarketplaceStatsService {
    async fn submit_quality_report(
        &self,
        _report: QualityReportData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Ok(())
    }

    async fn get_skill_stats(
        &self,
        skill_name: String,
    ) -> Result<SkillMarketplaceStats, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Marketplace stats unavailable (offline mode) for '{skill_name}'"),
        ))
    }

    async fn search_ranked(
        &self,
        _query: SkillSearchQuery,
    ) -> Result<SkillSearchResponse, (StatusCode, Json<ErrorResponse>)> {
        Ok(SkillSearchResponse {
            results: Vec::new(),
            total: 0,
            limit: 0,
            offset: 0,
        })
    }

    async fn refresh_aggregation(
        &self,
        _skill_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pagination::{MAX_MARKETPLACE_SEARCH_OFFSET, clamp_marketplace_search_offset};

    #[test]
    fn skill_search_query_default_all_none() {
        let q = SkillSearchQuery::default();
        assert!(q.query.is_none());
        assert!(q.category.is_none());
        assert!(q.trust_tier.is_none());
        assert!(q.limit.is_none());
        assert!(q.offset.is_none());
    }

    #[test]
    fn skill_search_query_from_json_partial() {
        let q: SkillSearchQuery = serde_json::from_str(r#"{"query":"test","limit":10}"#).unwrap();
        assert_eq!(q.query.as_deref(), Some("test"));
        assert_eq!(q.limit, Some(10));
        assert!(q.category.is_none());
    }

    #[test]
    fn skill_search_result_round_trip() {
        let r = SkillSearchResult {
            skill_name: "s1".into(),
            version: "1.0".into(),
            description: Some("desc".into()),
            publisher_id: None,
            trust_tier: Some("verified".into()),
            category: None,
            ranking_score: 0.95,
            avg_quality: 0.8,
            total_installs: 100,
            active_users_7d: 42,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: SkillSearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r.skill_name, back.skill_name);
        assert_eq!(r.ranking_score, back.ranking_score);
        assert_eq!(r.active_users_7d, back.active_users_7d);
    }

    #[test]
    fn skill_search_limit_clamps_to_max_results() {
        let lim = Some(u32::MAX)
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .min(super::MAX_SEARCH_RESULTS);
        assert_eq!(lim, super::MAX_SEARCH_RESULTS);
    }

    #[test]
    fn skill_search_offset_uses_shared_clamp() {
        assert_eq!(
            clamp_marketplace_search_offset(u32::MAX),
            MAX_MARKETPLACE_SEARCH_OFFSET
        );
    }
}
