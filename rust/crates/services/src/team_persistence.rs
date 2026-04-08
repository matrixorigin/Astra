//! Team definitions persistence — MatrixOne-backed storage for team configurations.
//!
//! Provides a [`TeamPersistenceService`] trait for CRUD operations and an in-memory
//! implementation for CLI / test use. Database-backed implementation uses the
//! `team_definitions` table in MatrixOne.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

use crate::coordination::{
    AgentProfile, AgentTier, AggregationStrategy, CoordinationPattern, DelegationRequest,
    PipelineStage,
};

// ─── Team Definition Types ──────────────────────────────────────────────────

/// Persistent team definition stored in MatrixOne.
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS team_definitions (
///     team_id       VARCHAR(64)  PRIMARY KEY,
///     user_id       VARCHAR(64)  NOT NULL,
///     name          VARCHAR(128) NOT NULL,
///     description   TEXT,
///     coordination  TEXT         NOT NULL,
///     members_json  TEXT         NOT NULL,
///     context_json  TEXT,
///     worktree_mode VARCHAR(32)  DEFAULT 'shared',
///     created_at    TIMESTAMP(6) DEFAULT NOW(6),
///     updated_at    TIMESTAMP(6) DEFAULT NOW(6),
///     UNIQUE KEY uq_team_user_name (user_id, name)
/// );
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamDefinition {
    pub team_id: String,
    pub user_id: String,
    pub name: String,
    pub description: String,
    pub coordination: TeamCoordination,
    pub members: Vec<TeamMemberDef>,
    pub context: HashMap<String, String>,
    pub worktree_mode: WorktreeMode,
    pub created_at: String,
    pub updated_at: String,
}

/// Coordination strategy for a team — maps to [`CoordinationPattern`] at execution time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeamCoordination {
    /// Producer + reviewer loop.
    Adversarial {
        max_rounds: u32,
        threshold: f64,
    },
    /// Parallel dispatch with aggregation.
    FanOut {
        aggregation: String,
    },
    /// Sequential chain: output of member N feeds member N+1.
    Pipeline,
    /// One-by-one with optional early exit.
    Sequential {
        stop_on_success: bool,
    },
}

/// Lightweight member declaration within a team.
///
/// Resolved to a full [`AgentProfile`] at execution time via [`resolve_member_to_profile`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMemberDef {
    pub role: String,
    /// Reference to an existing registered agent. When `None`, a transient
    /// profile is created from the role name.
    pub agent_id: Option<String>,
    pub system_prompt: Option<String>,
    pub skills: Vec<String>,
    pub model_override: Option<String>,
    pub mcp_servers: Vec<String>,
}

/// How the team's agents share the workspace file system.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeMode {
    /// All agents share the same working directory (current behaviour).
    Shared,
    /// Each agent gets an independent git worktree.
    Isolated,
    /// Agents work in a MatrixOne stage area; changes committed on success.
    Staged,
}

impl Default for WorktreeMode {
    fn default() -> Self {
        Self::Shared
    }
}

// ─── Resolve: TeamMemberDef → AgentProfile ──────────────────────────────────

/// Convert a team member declaration into a full [`AgentProfile`].
///
/// The generated profile is always `AgentTier::User` (cannot delegate) and
/// inherits the team description as context in its system prompt.
pub fn resolve_member_to_profile(member: &TeamMemberDef, team: &TeamDefinition) -> AgentProfile {
    let agent_id = member
        .agent_id
        .clone()
        .unwrap_or_else(|| format!("team-{}-{}", team.name, member.role));

    let system_prompt = member.system_prompt.clone().unwrap_or_else(|| {
        format!(
            "You are the {} in the \"{}\" team. Team description: {}",
            member.role, team.name, team.description
        )
    });

    let mut profile = AgentProfile::new(&agent_id, &member.role, AgentTier::User);
    profile.system_prompt = Some(system_prompt);
    profile.skill_filter = member.skills.clone();
    profile.model_override = member.model_override.clone();
    profile.mcp_servers = member.mcp_servers.clone();
    profile
}

// ─── Bridge: Team → DelegationRequest ───────────────────────────────────────

fn parse_aggregation(s: &str) -> AggregationStrategy {
    match s {
        "first_success" => AggregationStrategy::FirstSuccess,
        "consensus" => AggregationStrategy::Consensus,
        "all_results" | "concatenate" => AggregationStrategy::AllResults,
        other => AggregationStrategy::LlmGuided {
            prompt_template: other.to_string(),
        },
    }
}

/// Convert a [`TeamDefinition`] + task into a [`DelegationRequest`].
///
/// The caller is responsible for registering the resolved profiles in the
/// `AgentProfileRegistry` before executing the request.
pub fn team_to_delegation_request(
    team: &TeamDefinition,
    task: &str,
    parent_run_id: &str,
) -> (DelegationRequest, Vec<AgentProfile>) {
    let profiles: Vec<AgentProfile> = team
        .members
        .iter()
        .map(|m| resolve_member_to_profile(m, team))
        .collect();

    let pattern = match &team.coordination {
        TeamCoordination::Adversarial {
            max_rounds,
            threshold,
        } => {
            let producer_id = profiles
                .first()
                .map(|p| p.agent_id.clone())
                .unwrap_or_default();
            let reviewer_id = profiles
                .get(1)
                .map(|p| p.agent_id.clone())
                .unwrap_or_default();
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                max_rounds: *max_rounds,
                acceptance_threshold: *threshold,
            }
        }
        TeamCoordination::FanOut { aggregation } => CoordinationPattern::FanOut {
            agent_ids: profiles.iter().map(|p| p.agent_id.clone()).collect(),
            aggregation: parse_aggregation(aggregation),
            timeout_sec: 300,
        },
        TeamCoordination::Pipeline => CoordinationPattern::Pipeline {
            stages: profiles
                .iter()
                .map(|p| PipelineStage {
                    agent_id: p.agent_id.clone(),
                    output_transform: None,
                })
                .collect(),
        },
        TeamCoordination::Sequential { stop_on_success } => CoordinationPattern::Sequential {
            agent_ids: profiles.iter().map(|p| p.agent_id.clone()).collect(),
            stop_on_success: *stop_on_success,
        },
    };

    let context: HashMap<String, serde_json::Value> = team
        .context
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();

    let request = DelegationRequest {
        delegation_id: uuid::Uuid::new_v4().to_string(),
        parent_run_id: parent_run_id.to_string(),
        task: task.to_string(),
        pattern,
        user_id: team.user_id.clone(),
        depth: 0,
        context,
    };

    (request, profiles)
}

// ─── Persistence Trait ──────────────────────────────────────────────────────

/// CRUD operations for team definitions.
#[async_trait]
pub trait TeamPersistenceService: Send + Sync {
    async fn save_team(&self, team: &TeamDefinition) -> Result<(), String>;
    async fn load_team(&self, user_id: &str, name: &str) -> Result<Option<TeamDefinition>, String>;
    async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamDefinition>, String>;
    async fn delete_team(&self, user_id: &str, name: &str) -> Result<bool, String>;
}

// ─── In-Memory Implementation ───────────────────────────────────────────────

/// In-memory implementation suitable for CLI use and testing.
pub struct InMemoryTeamStore {
    teams: RwLock<HashMap<String, TeamDefinition>>,
}

impl InMemoryTeamStore {
    pub fn new() -> Self {
        Self {
            teams: RwLock::new(HashMap::new()),
        }
    }

    /// Create a store pre-populated with the three built-in teams.
    pub fn with_builtins(user_id: &str) -> Self {
        let store = Self::new();
        let now = chrono::Utc::now().to_rfc3339();
        let builtins = builtin_teams(user_id, &now);
        {
            let mut map = store.teams.write().unwrap();
            for t in builtins {
                let key = format!("{}:{}", t.user_id, t.name);
                map.insert(key, t);
            }
        }
        store
    }
}

impl Default for InMemoryTeamStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TeamPersistenceService for InMemoryTeamStore {
    async fn save_team(&self, team: &TeamDefinition) -> Result<(), String> {
        let key = format!("{}:{}", team.user_id, team.name);
        let mut map = self.teams.write().map_err(|e| e.to_string())?;
        map.insert(key, team.clone());
        Ok(())
    }

    async fn load_team(&self, user_id: &str, name: &str) -> Result<Option<TeamDefinition>, String> {
        let key = format!("{user_id}:{name}");
        let map = self.teams.read().map_err(|e| e.to_string())?;
        Ok(map.get(&key).cloned())
    }

    async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamDefinition>, String> {
        let prefix = format!("{user_id}:");
        let map = self.teams.read().map_err(|e| e.to_string())?;
        let mut teams: Vec<_> = map
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| v.clone())
            .collect();
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(teams)
    }

    async fn delete_team(&self, user_id: &str, name: &str) -> Result<bool, String> {
        let key = format!("{user_id}:{name}");
        let mut map = self.teams.write().map_err(|e| e.to_string())?;
        Ok(map.remove(&key).is_some())
    }
}

// ─── MatrixOne-backed Implementation ────────────────────────────────────────

/// Team persistence backed by MatrixOne's `team_definitions` table.
///
/// Uses sqlx connection pool with parameterized queries. The schema is created
/// by [`crate::storage::ensure_core_schema`].
///
/// Serialization: `coordination`, `members`, and `context` are stored as JSON
/// text columns. `worktree_mode` is stored as a lowercase string ("shared",
/// "isolated", "staged").
pub struct MatrixOneTeamStore {
    pool: sqlx::Pool<sqlx::MySql>,
}

impl MatrixOneTeamStore {
    /// Create from an existing connection pool.
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool }
    }

    /// Seed built-in teams for a user if they don't already exist.
    pub async fn ensure_builtins(&self, user_id: &str) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        for team in builtin_teams(user_id, &now) {
            let existing = self.load_team(user_id, &team.name).await?;
            if existing.is_none() {
                self.save_team(&team).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl TeamPersistenceService for MatrixOneTeamStore {
    async fn save_team(&self, team: &TeamDefinition) -> Result<(), String> {
        let coordination_json =
            serde_json::to_string(&team.coordination).map_err(|e| e.to_string())?;
        let members_json =
            serde_json::to_string(&team.members).map_err(|e| e.to_string())?;
        let context_json =
            serde_json::to_string(&team.context).map_err(|e| e.to_string())?;
        let worktree_str = serde_json::to_string(&team.worktree_mode)
            .map_err(|e| e.to_string())?
            .trim_matches('"')
            .to_string();

        // Upsert: try UPDATE first (by user_id + name), INSERT if no rows affected.
        let updated = sqlx::query(
            "UPDATE team_definitions SET \
                 team_id       = ?, \
                 description   = ?, \
                 coordination  = ?, \
                 members_json  = ?, \
                 context_json  = ?, \
                 worktree_mode = ?, \
                 updated_at    = NOW(6) \
             WHERE user_id = ? AND name = ?",
        )
        .bind(&team.team_id)
        .bind(&team.description)
        .bind(&coordination_json)
        .bind(&members_json)
        .bind(&context_json)
        .bind(&worktree_str)
        .bind(&team.user_id)
        .bind(&team.name)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("team UPDATE failed: {e}"))?;

        if updated.rows_affected() == 0 {
            sqlx::query(
                "INSERT INTO team_definitions \
                 (team_id, user_id, name, description, coordination, members_json, \
                  context_json, worktree_mode, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
            )
            .bind(&team.team_id)
            .bind(&team.user_id)
            .bind(&team.name)
            .bind(&team.description)
            .bind(&coordination_json)
            .bind(&members_json)
            .bind(&context_json)
            .bind(&worktree_str)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("team INSERT failed: {e}"))?;
        }

        Ok(())
    }

    async fn load_team(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<TeamDefinition>, String> {
        let row = sqlx::query(
            "SELECT team_id, user_id, name, description, coordination, \
                    members_json, context_json, worktree_mode, \
                    created_at, updated_at \
             FROM team_definitions WHERE user_id = ? AND name = ?",
        )
        .bind(user_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("team SELECT failed: {e}"))?;

        match row {
            None => Ok(None),
            Some(row) => {
                let team = row_to_team_definition(&row)?;
                Ok(Some(team))
            }
        }
    }

    async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamDefinition>, String> {
        let rows = sqlx::query(
            "SELECT team_id, user_id, name, description, coordination, \
                    members_json, context_json, worktree_mode, \
                    created_at, updated_at \
             FROM team_definitions WHERE user_id = ? ORDER BY name",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("team SELECT ALL failed: {e}"))?;

        let mut teams = Vec::with_capacity(rows.len());
        for row in &rows {
            teams.push(row_to_team_definition(row)?);
        }
        Ok(teams)
    }

    async fn delete_team(&self, user_id: &str, name: &str) -> Result<bool, String> {
        let result = sqlx::query(
            "DELETE FROM team_definitions WHERE user_id = ? AND name = ?",
        )
        .bind(user_id)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("team DELETE failed: {e}"))?;

        Ok(result.rows_affected() > 0)
    }
}

/// Parse a database row into a [`TeamDefinition`].
fn row_to_team_definition(row: &sqlx::mysql::MySqlRow) -> Result<TeamDefinition, String> {
    use sqlx::Row;

    let team_id: String = row.get("team_id");
    let user_id: String = row.get("user_id");
    let name: String = row.get("name");
    let description: String = row.try_get("description").unwrap_or_default();
    let coord_json: String = row.get("coordination");
    let members_str: String = row.get("members_json");
    let context_str: String = row.try_get("context_json").unwrap_or_default();
    let wt_str: String = row.try_get("worktree_mode").unwrap_or_else(|_| "shared".to_string());
    let created_at: String = row
        .try_get::<String, _>("created_at")
        .unwrap_or_default();
    let updated_at: String = row
        .try_get::<String, _>("updated_at")
        .unwrap_or_default();

    let coordination: TeamCoordination =
        serde_json::from_str(&coord_json).map_err(|e| format!("bad coordination JSON: {e}"))?;
    let members: Vec<TeamMemberDef> =
        serde_json::from_str(&members_str).map_err(|e| format!("bad members JSON: {e}"))?;
    let context: HashMap<String, String> = if context_str.is_empty() {
        HashMap::new()
    } else {
        serde_json::from_str(&context_str).map_err(|e| format!("bad context JSON: {e}"))?
    };
    let worktree_mode: WorktreeMode =
        serde_json::from_str(&format!("\"{wt_str}\"")).unwrap_or_default();

    Ok(TeamDefinition {
        team_id,
        user_id,
        name,
        description,
        coordination,
        members,
        context,
        worktree_mode,
        created_at,
        updated_at,
    })
}

// ─── Execution History ──────────────────────────────────────────────────────

/// A record of a team execution in the `team_execution_history` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamExecutionRecord {
    pub execution_id: String,
    pub team_id: String,
    pub user_id: String,
    pub task: String,
    pub status: String,
    pub result_json: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

impl MatrixOneTeamStore {
    /// Record the start of a team execution.
    pub async fn record_execution_start(
        &self,
        execution_id: &str,
        team_id: &str,
        user_id: &str,
        task: &str,
    ) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO team_execution_history \
             (execution_id, team_id, user_id, task, status, started_at) \
             VALUES (?, ?, ?, ?, 'running', NOW(6))",
        )
        .bind(execution_id)
        .bind(team_id)
        .bind(user_id)
        .bind(task)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("execution INSERT failed: {e}"))?;
        Ok(())
    }

    /// Record the completion of a team execution.
    pub async fn record_execution_complete(
        &self,
        execution_id: &str,
        status: &str,
        result_json: Option<&str>,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE team_execution_history SET \
                 status       = ?, \
                 result_json  = ?, \
                 completed_at = NOW(6) \
             WHERE execution_id = ?",
        )
        .bind(status)
        .bind(result_json)
        .bind(execution_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("execution UPDATE failed: {e}"))?;
        Ok(())
    }

    /// List execution history for a team, most recent first.
    pub async fn list_executions(
        &self,
        team_id: &str,
        limit: u32,
    ) -> Result<Vec<TeamExecutionRecord>, String> {
        use sqlx::Row;

        let rows = sqlx::query(
            "SELECT execution_id, team_id, user_id, task, status, \
                    result_json, started_at, completed_at \
             FROM team_execution_history \
             WHERE team_id = ? \
             ORDER BY started_at DESC \
             LIMIT ?",
        )
        .bind(team_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("execution SELECT failed: {e}"))?;

        let mut records = Vec::with_capacity(rows.len());
        for row in &rows {
            records.push(TeamExecutionRecord {
                execution_id: row.get("execution_id"),
                team_id: row.get("team_id"),
                user_id: row.get("user_id"),
                task: row.get("task"),
                status: row.get("status"),
                result_json: row.try_get("result_json").ok(),
                started_at: row.try_get::<String, _>("started_at").unwrap_or_default(),
                completed_at: row.try_get::<String, _>("completed_at").ok(),
            });
        }
        Ok(records)
    }
}

// ─── Built-in Teams ─────────────────────────────────────────────────────────

/// The three standard team templates: review, research, dev.
pub fn builtin_teams(user_id: &str, now: &str) -> Vec<TeamDefinition> {
    vec![
        TeamDefinition {
            team_id: format!("builtin-review-{user_id}"),
            user_id: user_id.to_string(),
            name: "review".to_string(),
            description: "Adversarial code review: one agent writes, another reviews".to_string(),
            coordination: TeamCoordination::Adversarial {
                max_rounds: 3,
                threshold: 0.8,
            },
            members: vec![
                TeamMemberDef {
                    role: "producer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You write or modify code to fulfil the task. \
                         Incorporate reviewer feedback in subsequent rounds."
                            .to_string(),
                    ),
                    skills: vec!["review-changes".to_string()],
                    model_override: None,
                    mcp_servers: vec![],
                },
                TeamMemberDef {
                    role: "reviewer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You review code for bugs, security issues, and correctness. \
                         Provide actionable feedback."
                            .to_string(),
                    ),
                    skills: vec!["review-changes".to_string()],
                    model_override: None,
                    mcp_servers: vec![],
                },
            ],
            context: HashMap::new(),
            worktree_mode: WorktreeMode::Shared,
            created_at: now.to_string(),
            updated_at: now.to_string(),
        },
        TeamDefinition {
            team_id: format!("builtin-research-{user_id}"),
            user_id: user_id.to_string(),
            name: "research".to_string(),
            description: "Deep research: explorer gathers info, synthesizer produces report"
                .to_string(),
            coordination: TeamCoordination::Pipeline,
            members: vec![
                TeamMemberDef {
                    role: "explorer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You search the codebase, read docs, and gather information. \
                         Output structured findings."
                            .to_string(),
                    ),
                    skills: vec!["analyze-session".to_string()],
                    model_override: None,
                    mcp_servers: vec![],
                },
                TeamMemberDef {
                    role: "synthesizer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You synthesize findings into a coherent analysis report.".to_string(),
                    ),
                    skills: vec![],
                    model_override: None,
                    mcp_servers: vec![],
                },
            ],
            context: HashMap::new(),
            worktree_mode: WorktreeMode::Shared,
            created_at: now.to_string(),
            updated_at: now.to_string(),
        },
        TeamDefinition {
            team_id: format!("builtin-dev-{user_id}"),
            user_id: user_id.to_string(),
            name: "dev".to_string(),
            description:
                "Full development cycle: planner decomposes, implementer codes, tester verifies"
                    .to_string(),
            coordination: TeamCoordination::Pipeline,
            members: vec![
                TeamMemberDef {
                    role: "planner".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You decompose the task into subtasks with acceptance criteria.".to_string(),
                    ),
                    skills: vec![],
                    model_override: None,
                    mcp_servers: vec![],
                },
                TeamMemberDef {
                    role: "implementer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You implement code changes following the plan.".to_string(),
                    ),
                    skills: vec![],
                    model_override: None,
                    mcp_servers: vec![],
                },
                TeamMemberDef {
                    role: "tester".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You write and run tests, verifying acceptance criteria.".to_string(),
                    ),
                    skills: vec!["verify-task".to_string()],
                    model_override: None,
                    mcp_servers: vec![],
                },
            ],
            context: HashMap::new(),
            worktree_mode: WorktreeMode::Isolated,
            created_at: now.to_string(),
            updated_at: now.to_string(),
        },
    ]
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_team() -> TeamDefinition {
        TeamDefinition {
            team_id: "test-team-1".to_string(),
            user_id: "user-1".to_string(),
            name: "test-team".to_string(),
            description: "A test team".to_string(),
            coordination: TeamCoordination::Pipeline,
            members: vec![
                TeamMemberDef {
                    role: "coder".to_string(),
                    agent_id: Some("coder-agent".to_string()),
                    system_prompt: None,
                    skills: vec!["edit".to_string()],
                    model_override: None,
                    mcp_servers: vec![],
                },
                TeamMemberDef {
                    role: "reviewer".to_string(),
                    agent_id: None,
                    system_prompt: Some("Review carefully".to_string()),
                    skills: vec!["review-changes".to_string()],
                    model_override: Some("claude-3-opus".to_string()),
                    mcp_servers: vec!["github".to_string()],
                },
            ],
            context: HashMap::from([("project".to_string(), "test-project".to_string())]),
            worktree_mode: WorktreeMode::Isolated,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    // ── Resolve ──

    #[test]
    fn resolve_member_with_explicit_agent_id() {
        let team = test_team();
        let profile = resolve_member_to_profile(&team.members[0], &team);
        assert_eq!(profile.agent_id, "coder-agent");
        assert_eq!(profile.skill_filter, vec!["edit"]);
        assert!(profile.model_override.is_none());
        assert_eq!(profile.tier, AgentTier::User);
        assert!(!profile.can_delegate);
    }

    #[test]
    fn resolve_member_auto_generates_agent_id() {
        let team = test_team();
        let profile = resolve_member_to_profile(&team.members[1], &team);
        assert_eq!(profile.agent_id, "team-test-team-reviewer");
        assert_eq!(profile.model_override.as_deref(), Some("claude-3-opus"));
        assert_eq!(profile.mcp_servers, vec!["github"]);
    }

    #[test]
    fn resolve_member_auto_prompt_includes_team_info() {
        let team = test_team();
        let profile = resolve_member_to_profile(&team.members[0], &team);
        let prompt = profile.system_prompt.unwrap();
        assert!(prompt.contains("coder"));
        assert!(prompt.contains("test-team"));
        assert!(prompt.contains("A test team"));
    }

    #[test]
    fn resolve_member_explicit_prompt_used() {
        let team = test_team();
        let profile = resolve_member_to_profile(&team.members[1], &team);
        assert_eq!(profile.system_prompt.as_deref(), Some("Review carefully"));
    }

    // ── Bridge ──

    #[test]
    fn team_to_delegation_pipeline() {
        let team = test_team();
        let (req, profiles) = team_to_delegation_request(&team, "Fix the bug", "run-parent");
        assert_eq!(profiles.len(), 2);
        assert_eq!(req.task, "Fix the bug");
        assert_eq!(req.user_id, "user-1");
        assert_eq!(req.depth, 0);
        match &req.pattern {
            CoordinationPattern::Pipeline { stages } => {
                assert_eq!(stages.len(), 2);
                assert_eq!(stages[0].agent_id, "coder-agent");
                assert_eq!(stages[1].agent_id, "team-test-team-reviewer");
            }
            other => panic!("expected Pipeline, got {other:?}"),
        }
    }

    #[test]
    fn team_to_delegation_adversarial() {
        let mut team = test_team();
        team.coordination = TeamCoordination::Adversarial {
            max_rounds: 5,
            threshold: 0.9,
        };
        let (req, _) = team_to_delegation_request(&team, "Review code", "run-1");
        match &req.pattern {
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                max_rounds,
                acceptance_threshold,
            } => {
                assert_eq!(producer_id, "coder-agent");
                assert_eq!(reviewer_id, "team-test-team-reviewer");
                assert_eq!(*max_rounds, 5);
                assert!((acceptance_threshold - 0.9).abs() < f64::EPSILON);
            }
            other => panic!("expected AdversarialReview, got {other:?}"),
        }
    }

    #[test]
    fn team_to_delegation_fan_out() {
        let mut team = test_team();
        team.coordination = TeamCoordination::FanOut {
            aggregation: "consensus".to_string(),
        };
        let (req, _) = team_to_delegation_request(&team, "Analyze", "run-1");
        match &req.pattern {
            CoordinationPattern::FanOut {
                agent_ids,
                aggregation,
                timeout_sec,
            } => {
                assert_eq!(agent_ids.len(), 2);
                assert!(matches!(aggregation, AggregationStrategy::Consensus));
                assert_eq!(*timeout_sec, 300);
            }
            other => panic!("expected FanOut, got {other:?}"),
        }
    }

    #[test]
    fn team_to_delegation_sequential() {
        let mut team = test_team();
        team.coordination = TeamCoordination::Sequential {
            stop_on_success: true,
        };
        let (req, _) = team_to_delegation_request(&team, "Try fixes", "run-1");
        match &req.pattern {
            CoordinationPattern::Sequential {
                agent_ids,
                stop_on_success,
            } => {
                assert_eq!(agent_ids.len(), 2);
                assert!(*stop_on_success);
            }
            other => panic!("expected Sequential, got {other:?}"),
        }
    }

    #[test]
    fn team_context_injected_into_request() {
        let team = test_team();
        let (req, _) = team_to_delegation_request(&team, "task", "run-1");
        assert_eq!(
            req.context.get("project"),
            Some(&serde_json::Value::String("test-project".to_string()))
        );
    }

    // ── Persistence ──

    #[tokio::test]
    async fn in_memory_store_crud() {
        let store = InMemoryTeamStore::new();
        let team = test_team();

        // Save
        store.save_team(&team).await.unwrap();

        // Load
        let loaded = store.load_team("user-1", "test-team").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().team_id, "test-team-1");

        // List
        let list = store.list_teams("user-1").await.unwrap();
        assert_eq!(list.len(), 1);

        // Delete
        let deleted = store.delete_team("user-1", "test-team").await.unwrap();
        assert!(deleted);

        // Verify gone
        let gone = store.load_team("user-1", "test-team").await.unwrap();
        assert!(gone.is_none());
    }

    #[tokio::test]
    async fn in_memory_store_with_builtins() {
        let store = InMemoryTeamStore::with_builtins("u1");
        let teams = store.list_teams("u1").await.unwrap();
        assert_eq!(teams.len(), 3);
        let names: Vec<_> = teams.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"review"));
        assert!(names.contains(&"research"));
        assert!(names.contains(&"dev"));
    }

    #[tokio::test]
    async fn in_memory_store_user_isolation() {
        let store = InMemoryTeamStore::with_builtins("u1");
        let teams = store.list_teams("u2").await.unwrap();
        assert!(teams.is_empty());
    }

    // ── Builtins ──

    #[test]
    fn builtin_review_team_is_adversarial() {
        let teams = builtin_teams("u1", "2026-01-01T00:00:00Z");
        let review = teams.iter().find(|t| t.name == "review").unwrap();
        assert!(matches!(
            review.coordination,
            TeamCoordination::Adversarial { .. }
        ));
        assert_eq!(review.members.len(), 2);
    }

    #[test]
    fn builtin_dev_team_is_pipeline_with_isolated_worktree() {
        let teams = builtin_teams("u1", "2026-01-01T00:00:00Z");
        let dev = teams.iter().find(|t| t.name == "dev").unwrap();
        assert_eq!(dev.coordination, TeamCoordination::Pipeline);
        assert_eq!(dev.worktree_mode, WorktreeMode::Isolated);
        assert_eq!(dev.members.len(), 3);
    }

    #[test]
    fn worktree_mode_serde_roundtrip() {
        let json = serde_json::to_string(&WorktreeMode::Isolated).unwrap();
        assert_eq!(json, "\"isolated\"");
        let parsed: WorktreeMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, WorktreeMode::Isolated);
    }

    #[test]
    fn team_coordination_serde_roundtrip() {
        let coord = TeamCoordination::Adversarial {
            max_rounds: 3,
            threshold: 0.8,
        };
        let json = serde_json::to_string(&coord).unwrap();
        let parsed: TeamCoordination = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, coord);
    }

    #[test]
    fn aggregation_parsing() {
        assert!(matches!(
            parse_aggregation("first_success"),
            AggregationStrategy::FirstSuccess
        ));
        assert!(matches!(
            parse_aggregation("consensus"),
            AggregationStrategy::Consensus
        ));
        assert!(matches!(
            parse_aggregation("all_results"),
            AggregationStrategy::AllResults
        ));
        assert!(matches!(
            parse_aggregation("concatenate"),
            AggregationStrategy::AllResults
        ));
        assert!(matches!(
            parse_aggregation("custom prompt"),
            AggregationStrategy::LlmGuided { .. }
        ));
    }

    // ── MatrixOne serialization helpers ──

    #[test]
    fn coordination_json_roundtrips_through_matrixone_format() {
        // Simulate what MatrixOneTeamStore does: serialize to JSON text, store, deserialize
        let coords = vec![
            TeamCoordination::Adversarial {
                max_rounds: 5,
                threshold: 0.85,
            },
            TeamCoordination::FanOut {
                aggregation: "consensus".to_string(),
            },
            TeamCoordination::Pipeline,
            TeamCoordination::Sequential {
                stop_on_success: true,
            },
        ];

        for coord in &coords {
            let json = serde_json::to_string(coord).unwrap();
            let parsed: TeamCoordination = serde_json::from_str(&json).unwrap();
            assert_eq!(*coord, parsed, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn members_json_roundtrip() {
        let members = vec![
            TeamMemberDef {
                role: "coder".to_string(),
                agent_id: Some("my-coder".to_string()),
                system_prompt: None,
                skills: vec!["edit".to_string(), "test".to_string()],
                model_override: Some("gpt-4".to_string()),
                mcp_servers: vec!["github".to_string()],
            },
            TeamMemberDef {
                role: "reviewer".to_string(),
                agent_id: None,
                system_prompt: Some("Be thorough".to_string()),
                skills: vec![],
                model_override: None,
                mcp_servers: vec![],
            },
        ];

        let json = serde_json::to_string(&members).unwrap();
        let parsed: Vec<TeamMemberDef> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].role, "coder");
        assert_eq!(parsed[0].agent_id.as_deref(), Some("my-coder"));
        assert_eq!(parsed[1].system_prompt.as_deref(), Some("Be thorough"));
    }

    #[test]
    fn worktree_mode_string_format() {
        // MatrixOneTeamStore stores worktree_mode as a bare string, not JSON
        for (mode, expected) in [
            (WorktreeMode::Shared, "shared"),
            (WorktreeMode::Isolated, "isolated"),
            (WorktreeMode::Staged, "staged"),
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            // serde produces "\"shared\"", trim quotes for DB storage
            let bare = json.trim_matches('"');
            assert_eq!(bare, expected);
            // Reverse: wrap in quotes for deserialization
            let restored: WorktreeMode =
                serde_json::from_str(&format!("\"{bare}\"")).unwrap();
            assert_eq!(restored, mode);
        }
    }

    #[test]
    fn context_json_handles_empty() {
        let empty: HashMap<String, String> = HashMap::new();
        let json = serde_json::to_string(&empty).unwrap();
        assert_eq!(json, "{}");
        let parsed: HashMap<String, String> = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn team_execution_record_serde_roundtrip() {
        let record = TeamExecutionRecord {
            execution_id: "exec-1".to_string(),
            team_id: "team-1".to_string(),
            user_id: "user-1".to_string(),
            task: "Fix auth bug".to_string(),
            status: "completed".to_string(),
            result_json: Some(r#"{"merged":true}"#.to_string()),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            completed_at: Some("2026-01-01T00:05:00Z".to_string()),
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: TeamExecutionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.execution_id, "exec-1");
        assert_eq!(parsed.status, "completed");
        assert!(parsed.completed_at.is_some());
    }

    #[test]
    fn full_team_definition_serde_roundtrip() {
        let team = test_team();
        let json = serde_json::to_string(&team).unwrap();
        let parsed: TeamDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.team_id, team.team_id);
        assert_eq!(parsed.name, team.name);
        assert_eq!(parsed.worktree_mode, WorktreeMode::Isolated);
        assert_eq!(parsed.members.len(), 2);
        assert!(parsed.context.contains_key("project"));
    }
}
