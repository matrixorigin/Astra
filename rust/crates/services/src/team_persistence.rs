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
    AgentProfile, AgentProfileRegistry, AgentTier, AggregationStrategy, CoordinationPattern,
    DelegationRequest, PipelineStage,
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
///     budget_json   TEXT,
///     max_parallel  INT UNSIGNED NOT NULL DEFAULT 0,
///     created_at    DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
///     updated_at    DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
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
    /// Optional budget constraints for the team execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<TeamBudget>,
    /// Maximum number of agents that may execute concurrently (0 = unlimited).
    #[serde(default)]
    pub max_parallel: u32,
    pub created_at: String,
    pub updated_at: String,
}

/// Budget constraints applied to a team execution.
///
/// `max_duration_secs` is enforced as a hard timeout (execution is cancelled).
/// `max_tokens` and `max_cost_usd` are **post-execution checks** — the execution
/// runs to completion and the budget violation is reported afterward, because
/// token counts are only known after LLM responses arrive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamBudget {
    /// Maximum cost in USD for the entire team execution.
    #[serde(default)]
    pub max_cost_usd: f64,
    /// Maximum total tokens (prompt + completion) across all agents.
    #[serde(default)]
    pub max_tokens: u64,
    /// Maximum wall-clock time in seconds for the entire execution.
    #[serde(default)]
    pub max_duration_secs: u64,
}

/// Coordination strategy for a team — maps to [`CoordinationPattern`] at execution time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeamCoordination {
    /// Producer + reviewer loop.
    Adversarial { max_rounds: u32, threshold: f64 },
    /// Parallel dispatch with aggregation.
    FanOut { aggregation: String },
    /// Sequential chain: output of member N feeds member N+1.
    Pipeline,
    /// One-by-one with optional early exit.
    Sequential { stop_on_success: bool },
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
    /// Whether this member can delegate to sub-agents.  
    /// Defaults to `false` (User tier, no delegation).
    #[serde(default)]
    pub can_delegate: bool,
    /// Maximum delegation depth for this member.
    /// Only meaningful when `can_delegate` is true.
    #[serde(default)]
    pub max_delegation_depth: u32,
}

/// How the team's agents share the workspace file system.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeMode {
    /// All agents share the same working directory (current behaviour).
    #[default]
    Shared,
    /// Each agent gets an independent git worktree.
    Isolated,
    /// Agents work in a MatrixOne stage area; changes committed on success.
    Staged,
}

// ─── Resolve: TeamMemberDef → AgentProfile ──────────────────────────────────

/// Convert a team member declaration into a full [`AgentProfile`].
///
/// When `registry` is provided and `member.agent_id` matches an existing profile,
/// the registered profile is used as a base — member fields override where set.
///
/// The generated profile inherits the team description as context in its system
/// prompt and stores team context in `metadata["team_context"]`.
pub fn resolve_member_to_profile(member: &TeamMemberDef, team: &TeamDefinition) -> AgentProfile {
    resolve_member_to_profile_with_registry(member, team, None)
}

/// Resolve with optional registry lookup.
///
/// If `registry` contains a profile matching `member.agent_id`, that profile is
/// used as the base and member-level overrides are applied on top.
pub fn resolve_member_to_profile_with_registry(
    member: &TeamMemberDef,
    team: &TeamDefinition,
    registry: Option<&AgentProfileRegistry>,
) -> AgentProfile {
    let agent_id = member
        .agent_id
        .clone()
        .unwrap_or_else(|| format!("team-{}-{}", team.name, member.role));

    // Try to use an existing registered profile as the base
    let mut profile = registry
        .and_then(|r| r.get(&agent_id))
        .cloned()
        .unwrap_or_else(|| {
            let tier = if member.can_delegate {
                AgentTier::System
            } else {
                AgentTier::User
            };
            AgentProfile::new(&agent_id, &member.role, tier)
        });

    // Apply member-level overrides
    let system_prompt = member.system_prompt.clone().unwrap_or_else(|| {
        profile.system_prompt.clone().unwrap_or_else(|| {
            format!(
                "You are the {} in the \"{}\" team. Team description: {}",
                member.role, team.name, team.description
            )
        })
    });
    profile.system_prompt = Some(system_prompt);

    if !member.skills.is_empty() {
        profile.skill_filter = member.skills.clone();
    }
    if member.model_override.is_some() {
        profile.model_override = member.model_override.clone();
    }
    if !member.mcp_servers.is_empty() {
        profile.mcp_servers = member.mcp_servers.clone();
    }

    // Override delegation settings from member def
    if member.can_delegate {
        profile.can_delegate = true;
        profile.tier = AgentTier::System;
        if member.max_delegation_depth > 0 {
            profile.max_delegation_depth = member.max_delegation_depth;
        }
    }

    // Inject team context into profile metadata
    if !team.context.is_empty() {
        profile.metadata.insert(
            "team_context".to_string(),
            serde_json::to_value(&team.context).unwrap_or_default(),
        );
    }
    profile.metadata.insert(
        "team_name".to_string(),
        serde_json::Value::String(team.name.clone()),
    );
    profile.metadata.insert(
        "team_role".to_string(),
        serde_json::Value::String(member.role.clone()),
    );

    profile
}

// ─── Team Validation ────────────────────────────────────────────────────────

/// Validation errors for a team definition.
#[derive(Debug, Clone, PartialEq)]
pub enum TeamValidationError {
    /// Adversarial requires exactly 2 members.
    AdversarialMemberCount(usize),
    /// Pipeline/Sequential requires at least 1 member.
    EmptyMembers,
    /// Duplicate role names within the same team.
    DuplicateRoles(Vec<String>),
    /// Duplicate agent IDs (explicit or generated).
    DuplicateAgentIds(Vec<String>),
    /// Budget contains invalid values.
    InvalidBudget(String),
}

impl std::fmt::Display for TeamValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AdversarialMemberCount(n) => {
                write!(
                    f,
                    "adversarial coordination requires exactly 2 members, got {n}"
                )
            }
            Self::EmptyMembers => write!(f, "team must have at least one member"),
            Self::DuplicateRoles(roles) => {
                write!(f, "duplicate roles: {}", roles.join(", "))
            }
            Self::DuplicateAgentIds(ids) => {
                write!(f, "duplicate agent IDs: {}", ids.join(", "))
            }
            Self::InvalidBudget(msg) => {
                write!(f, "invalid budget: {msg}")
            }
        }
    }
}

/// Validate a team definition before execution.
///
/// Checks:
/// - Non-empty member list
/// - Adversarial coordination requires exactly 2 members
/// - No duplicate roles
/// - No duplicate agent IDs (after resolution)
pub fn validate_team(team: &TeamDefinition) -> Result<(), Vec<TeamValidationError>> {
    let mut errors = Vec::new();

    if team.members.is_empty() {
        errors.push(TeamValidationError::EmptyMembers);
        return Err(errors);
    }

    if matches!(team.coordination, TeamCoordination::Adversarial { .. }) && team.members.len() != 2
    {
        errors.push(TeamValidationError::AdversarialMemberCount(
            team.members.len(),
        ));
    }

    // Check duplicate roles
    let mut role_counts: HashMap<&str, usize> = HashMap::new();
    for m in &team.members {
        *role_counts.entry(&m.role).or_default() += 1;
    }
    let dup_roles: Vec<String> = role_counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(role, _)| role.to_string())
        .collect();
    if !dup_roles.is_empty() {
        errors.push(TeamValidationError::DuplicateRoles(dup_roles));
    }

    // Check duplicate agent IDs
    let mut id_counts: HashMap<String, usize> = HashMap::new();
    for m in &team.members {
        let id = m
            .agent_id
            .clone()
            .unwrap_or_else(|| format!("team-{}-{}", team.name, m.role));
        *id_counts.entry(id).or_default() += 1;
    }
    let dup_ids: Vec<String> = id_counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(id, _)| id)
        .collect();
    if !dup_ids.is_empty() {
        errors.push(TeamValidationError::DuplicateAgentIds(dup_ids));
    }

    // Budget validation
    if let Some(budget) = &team.budget {
        if !budget.max_cost_usd.is_finite() {
            errors.push(TeamValidationError::InvalidBudget(
                "max_cost_usd must be a finite number".into(),
            ));
        } else if budget.max_cost_usd < 0.0 {
            errors.push(TeamValidationError::InvalidBudget(
                "max_cost_usd must be non-negative".into(),
            ));
        }
        if budget.max_cost_usd == 0.0 && budget.max_tokens == 0 && budget.max_duration_secs == 0 {
            errors.push(TeamValidationError::InvalidBudget(
                "budget specified but all limits are zero (no work possible)".into(),
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ─── Bulk Resolve ───────────────────────────────────────────────────────────

/// Resolve all team members, validate, and produce profiles + delegation request.
///
/// This is the high-level entry point for team execution. It:
/// 1. Validates the team definition
/// 2. Resolves all members to profiles (with optional registry lookup)
/// 3. Builds the delegation request
/// 4. Returns everything needed for the orchestrator
pub fn resolve_team(
    team: &TeamDefinition,
    task: &str,
    parent_run_id: &str,
    registry: Option<&AgentProfileRegistry>,
) -> Result<(DelegationRequest, Vec<AgentProfile>), String> {
    // Validate first
    validate_team(team).map_err(|errs| {
        errs.iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })?;

    // Resolve with optional registry
    let profiles: Vec<AgentProfile> = team
        .members
        .iter()
        .map(|m| resolve_member_to_profile_with_registry(m, team, registry))
        .collect();

    let pattern = build_coordination_pattern(&team.coordination, &profiles);

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

    Ok((request, profiles))
}

fn build_coordination_pattern(
    coordination: &TeamCoordination,
    profiles: &[AgentProfile],
) -> CoordinationPattern {
    match coordination {
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
    }
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
/// **Prefer [`resolve_team`]** for new code — it validates and supports registry lookup.
/// This function is kept for backward compatibility.
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

    let pattern = build_coordination_pattern(&team.coordination, &profiles);

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

/// CRUD operations for team definitions, execution history, and snapshots.
#[async_trait]
pub trait TeamPersistenceService: Send + Sync {
    async fn save_team(&self, team: &TeamDefinition) -> Result<(), String>;
    async fn load_team(&self, user_id: &str, name: &str) -> Result<Option<TeamDefinition>, String>;
    async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamDefinition>, String>;
    async fn delete_team(&self, user_id: &str, name: &str) -> Result<bool, String>;

    // ── Execution history ───────────────────────────────────────

    /// Record the start of a team execution. Default: no-op.
    async fn record_execution_start(
        &self,
        _execution_id: &str,
        _team_id: &str,
        _user_id: &str,
        _task: &str,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Record completion of a team execution. Default: no-op.
    async fn record_execution_complete(
        &self,
        _execution_id: &str,
        _status: &str,
        _result_json: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }

    /// List execution history for a team. Default: empty.
    async fn list_executions(
        &self,
        _team_id: &str,
        _limit: u32,
    ) -> Result<Vec<TeamExecutionRecord>, String> {
        Ok(vec![])
    }

    // ── Snapshots ───────────────────────────────────────────────

    /// Save a team snapshot. Default: no-op.
    async fn save_snapshot(&self, _snapshot: &TeamSnapshotRecord) -> Result<(), String> {
        Ok(())
    }

    /// List snapshots for a team, most recent first. Default: empty.
    async fn list_snapshots(
        &self,
        _team_name: &str,
        _user_id: &str,
        _limit: u32,
    ) -> Result<Vec<TeamSnapshotRecord>, String> {
        Ok(vec![])
    }

    /// Find a snapshot by exact ID or unique prefix. Default: None.
    async fn find_snapshot(
        &self,
        _snapshot_id: &str,
        _user_id: &str,
    ) -> Result<Option<TeamSnapshotRecord>, String> {
        Ok(None)
    }

    /// Delete a snapshot by ID. Returns true if found and deleted.
    async fn delete_snapshot(&self, _snapshot_id: &str, _user_id: &str) -> Result<bool, String> {
        Ok(false)
    }
}

// ─── In-Memory Implementation ───────────────────────────────────────────────

/// In-memory implementation suitable for CLI use and testing.
pub struct InMemoryTeamStore {
    teams: RwLock<HashMap<String, TeamDefinition>>,
    executions: RwLock<Vec<TeamExecutionRecord>>,
    snapshots: RwLock<Vec<TeamSnapshotRecord>>,
}

impl InMemoryTeamStore {
    pub fn new() -> Self {
        Self {
            teams: RwLock::new(HashMap::new()),
            executions: RwLock::new(Vec::new()),
            snapshots: RwLock::new(Vec::new()),
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

    // ── Execution history ───────────────────────────────────────

    async fn record_execution_start(
        &self,
        execution_id: &str,
        team_id: &str,
        user_id: &str,
        task: &str,
    ) -> Result<(), String> {
        let mut execs = self.executions.write().map_err(|e| e.to_string())?;

        // Retain at most 100 completed records per team to prevent unbounded growth.
        const MAX_COMPLETED_PER_TEAM: usize = 100;
        let completed_count = execs
            .iter()
            .filter(|e| e.team_id == team_id && e.completed_at.is_some())
            .count();
        if completed_count >= MAX_COMPLETED_PER_TEAM {
            // Remove oldest completed records for this team (keep running ones)
            let mut removed = 0;
            let to_remove = completed_count - MAX_COMPLETED_PER_TEAM + 1;
            execs.retain(|e| {
                if removed < to_remove
                    && e.team_id == team_id
                    && e.completed_at.is_some()
                {
                    removed += 1;
                    false
                } else {
                    true
                }
            });
        }

        execs.push(TeamExecutionRecord {
            execution_id: execution_id.to_string(),
            team_id: team_id.to_string(),
            user_id: user_id.to_string(),
            task: task.to_string(),
            status: "running".to_string(),
            result_json: None,
            started_at: chrono::Utc::now().to_rfc3339(),
            completed_at: None,
        });
        Ok(())
    }

    async fn record_execution_complete(
        &self,
        execution_id: &str,
        status: &str,
        result_json: Option<&str>,
    ) -> Result<(), String> {
        let mut execs = self.executions.write().map_err(|e| e.to_string())?;
        if let Some(rec) = execs.iter_mut().find(|r| r.execution_id == execution_id) {
            rec.status = status.to_string();
            rec.result_json = result_json.map(|s| s.to_string());
            rec.completed_at = Some(chrono::Utc::now().to_rfc3339());
        }
        Ok(())
    }

    async fn list_executions(
        &self,
        team_id: &str,
        limit: u32,
    ) -> Result<Vec<TeamExecutionRecord>, String> {
        let execs = self.executions.read().map_err(|e| e.to_string())?;
        let mut matching: Vec<_> = execs
            .iter()
            .filter(|r| r.team_id == team_id)
            .cloned()
            .collect();
        matching.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        matching.truncate(limit as usize);
        Ok(matching)
    }

    // ── Snapshots ───────────────────────────────────────────────

    async fn save_snapshot(&self, snapshot: &TeamSnapshotRecord) -> Result<(), String> {
        let mut snaps = self.snapshots.write().map_err(|e| e.to_string())?;
        snaps.push(snapshot.clone());
        Ok(())
    }

    async fn list_snapshots(
        &self,
        team_name: &str,
        user_id: &str,
        limit: u32,
    ) -> Result<Vec<TeamSnapshotRecord>, String> {
        let snaps = self.snapshots.read().map_err(|e| e.to_string())?;
        let mut matching: Vec<_> = snaps
            .iter()
            .filter(|s| s.team_name == team_name && s.user_id == user_id)
            .cloned()
            .collect();
        matching.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        matching.truncate(limit as usize);
        Ok(matching)
    }

    async fn find_snapshot(
        &self,
        snapshot_id: &str,
        user_id: &str,
    ) -> Result<Option<TeamSnapshotRecord>, String> {
        let snaps = self.snapshots.read().map_err(|e| e.to_string())?;
        // Exact match first
        if let Some(s) = snaps
            .iter()
            .find(|s| s.snapshot_id == snapshot_id && s.user_id == user_id)
        {
            return Ok(Some(s.clone()));
        }
        // Prefix match — return only if unique
        let matches: Vec<_> = snaps
            .iter()
            .filter(|s| s.user_id == user_id && s.snapshot_id.starts_with(snapshot_id))
            .collect();
        match matches.len() {
            1 => Ok(Some(matches[0].clone())),
            _ => Ok(None),
        }
    }

    async fn delete_snapshot(&self, snapshot_id: &str, user_id: &str) -> Result<bool, String> {
        let mut snaps = self.snapshots.write().map_err(|e| e.to_string())?;
        let before = snaps.len();
        snaps.retain(|s| !(s.snapshot_id == snapshot_id && s.user_id == user_id));
        Ok(snaps.len() < before)
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
        let members_json = serde_json::to_string(&team.members).map_err(|e| e.to_string())?;
        let context_json = serde_json::to_string(&team.context).map_err(|e| e.to_string())?;
        let worktree_str = serde_json::to_string(&team.worktree_mode)
            .map_err(|e| e.to_string())?
            .trim_matches('"')
            .to_string();
        let budget_json: Option<String> = team
            .budget
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| e.to_string())?;

        // Upsert: try UPDATE first (by user_id + name), INSERT if no rows affected.
        let updated = sqlx::query(
            "UPDATE team_definitions SET \
                 team_id       = ?, \
                 description   = ?, \
                 coordination  = ?, \
                 members_json  = ?, \
                 context_json  = ?, \
                 worktree_mode = ?, \
                 budget_json   = ?, \
                 max_parallel  = ?, \
                 updated_at    = NOW(6) \
             WHERE user_id = ? AND name = ?",
        )
        .bind(&team.team_id)
        .bind(&team.description)
        .bind(&coordination_json)
        .bind(&members_json)
        .bind(&context_json)
        .bind(&worktree_str)
        .bind(&budget_json)
        .bind(team.max_parallel)
        .bind(&team.user_id)
        .bind(&team.name)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("team UPDATE failed: {e}"))?;

        if updated.rows_affected() == 0 {
            sqlx::query(
                "INSERT INTO team_definitions \
                 (team_id, user_id, name, description, coordination, members_json, \
                  context_json, worktree_mode, budget_json, max_parallel, \
                  created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
            )
            .bind(&team.team_id)
            .bind(&team.user_id)
            .bind(&team.name)
            .bind(&team.description)
            .bind(&coordination_json)
            .bind(&members_json)
            .bind(&context_json)
            .bind(&worktree_str)
            .bind(&budget_json)
            .bind(team.max_parallel)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("team INSERT failed: {e}"))?;
        }

        Ok(())
    }

    async fn load_team(&self, user_id: &str, name: &str) -> Result<Option<TeamDefinition>, String> {
        let row = sqlx::query(
            "SELECT team_id, user_id, name, description, coordination, \
                    members_json, context_json, worktree_mode, \
                    budget_json, max_parallel, created_at, updated_at \
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
                    budget_json, max_parallel, created_at, updated_at \
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
        let result = sqlx::query("DELETE FROM team_definitions WHERE user_id = ? AND name = ?")
            .bind(user_id)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("team DELETE failed: {e}"))?;

        Ok(result.rows_affected() > 0)
    }

    // ── Execution history ───────────────────────────────────────

    async fn record_execution_start(
        &self,
        execution_id: &str,
        team_id: &str,
        user_id: &str,
        task: &str,
    ) -> Result<(), String> {
        // Retention: prune oldest completed rows beyond limit.
        // Only completed records are pruned — running records are preserved.
        const MAX_COMPLETED_PER_TEAM: u32 = 100;
        sqlx::query(
            "DELETE FROM team_execution_history \
             WHERE team_id = ? AND status != 'running' AND execution_id NOT IN ( \
                 SELECT execution_id FROM ( \
                     SELECT execution_id FROM team_execution_history \
                     WHERE team_id = ? AND status != 'running' \
                     ORDER BY started_at DESC \
                     LIMIT ? \
                 ) AS recent \
             )",
        )
        .bind(team_id)
        .bind(team_id)
        .bind(MAX_COMPLETED_PER_TEAM)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("execution retention prune failed: {e}"))?;

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

    async fn record_execution_complete(
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

    async fn list_executions(
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

    // ── Snapshots ───────────────────────────────────────────────

    async fn save_snapshot(&self, snapshot: &TeamSnapshotRecord) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO team_snapshots \
             (snapshot_id, team_name, user_id, label, git_commit, session_id, \
              team_definition_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&snapshot.snapshot_id)
        .bind(&snapshot.team_name)
        .bind(&snapshot.user_id)
        .bind(&snapshot.label)
        .bind(&snapshot.git_commit)
        .bind(&snapshot.session_id)
        .bind(&snapshot.team_definition_json)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("snapshot INSERT failed: {e}"))?;
        Ok(())
    }

    async fn list_snapshots(
        &self,
        team_name: &str,
        user_id: &str,
        limit: u32,
    ) -> Result<Vec<TeamSnapshotRecord>, String> {
        use sqlx::Row;

        let rows = sqlx::query(
            "SELECT snapshot_id, team_name, user_id, label, git_commit, \
                    session_id, team_definition_json, created_at \
             FROM team_snapshots \
             WHERE user_id = ? AND team_name = ? \
             ORDER BY created_at DESC \
             LIMIT ?",
        )
        .bind(user_id)
        .bind(team_name)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("snapshot SELECT failed: {e}"))?;

        let mut records = Vec::with_capacity(rows.len());
        for row in &rows {
            records.push(TeamSnapshotRecord {
                snapshot_id: row.get("snapshot_id"),
                team_name: row.get("team_name"),
                user_id: row.get("user_id"),
                label: row.try_get::<String, _>("label").unwrap_or_default(),
                git_commit: row.try_get("git_commit").ok(),
                session_id: row.try_get("session_id").ok(),
                team_definition_json: row.try_get("team_definition_json").ok(),
                created_at: row.try_get::<String, _>("created_at").unwrap_or_default(),
            });
        }
        Ok(records)
    }

    async fn find_snapshot(
        &self,
        snapshot_id: &str,
        user_id: &str,
    ) -> Result<Option<TeamSnapshotRecord>, String> {
        use sqlx::Row;

        let row = sqlx::query(
            "SELECT snapshot_id, team_name, user_id, label, git_commit, \
                    session_id, team_definition_json, created_at \
             FROM team_snapshots \
             WHERE snapshot_id = ? AND user_id = ?",
        )
        .bind(snapshot_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("snapshot SELECT failed: {e}"))?;

        Ok(row.map(|r| TeamSnapshotRecord {
            snapshot_id: r.get("snapshot_id"),
            team_name: r.get("team_name"),
            user_id: r.get("user_id"),
            label: r.try_get::<String, _>("label").unwrap_or_default(),
            git_commit: r.try_get("git_commit").ok(),
            session_id: r.try_get("session_id").ok(),
            team_definition_json: r.try_get("team_definition_json").ok(),
            created_at: r.try_get::<String, _>("created_at").unwrap_or_default(),
        }))
    }

    async fn delete_snapshot(&self, snapshot_id: &str, user_id: &str) -> Result<bool, String> {
        let result =
            sqlx::query("DELETE FROM team_snapshots WHERE snapshot_id = ? AND user_id = ?")
                .bind(snapshot_id)
                .bind(user_id)
                .execute(&self.pool)
                .await
                .map_err(|e| format!("snapshot DELETE failed: {e}"))?;
        Ok(result.rows_affected() > 0)
    }
}
fn row_to_team_definition(row: &sqlx::mysql::MySqlRow) -> Result<TeamDefinition, String> {
    use sqlx::Row;

    let team_id: String = row.get("team_id");
    let user_id: String = row.get("user_id");
    let name: String = row.get("name");
    let description: String = row.try_get("description").unwrap_or_default();
    let coord_json: String = row.get("coordination");
    let members_str: String = row.get("members_json");
    let context_str: String = row.try_get("context_json").unwrap_or_default();
    let wt_str: String = row
        .try_get("worktree_mode")
        .unwrap_or_else(|_| "shared".to_string());
    let budget_str: Option<String> = row.try_get("budget_json").unwrap_or(None);
    let max_parallel: u32 = row.try_get::<u32, _>("max_parallel").unwrap_or(0);
    let created_at: String = row.try_get::<String, _>("created_at").unwrap_or_default();
    let updated_at: String = row.try_get::<String, _>("updated_at").unwrap_or_default();

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
    let budget: Option<TeamBudget> = budget_str
        .filter(|s| !s.is_empty())
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|e| format!("bad budget JSON: {e}"))?;

    Ok(TeamDefinition {
        team_id,
        user_id,
        name,
        description,
        coordination,
        members,
        context,
        worktree_mode,
        budget,
        max_parallel,
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

/// A team snapshot record, capturing team state + git commit for restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamSnapshotRecord {
    pub snapshot_id: String,
    pub team_name: String,
    pub user_id: String,
    pub label: String,
    pub git_commit: Option<String>,
    pub session_id: Option<String>,
    pub team_definition_json: Option<String>,
    pub created_at: String,
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
                    can_delegate: false,
                    max_delegation_depth: 0,
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
                    can_delegate: false,
                    max_delegation_depth: 0,
                },
            ],
            context: HashMap::new(),
            worktree_mode: WorktreeMode::Shared,
            budget: None,
            max_parallel: 0,
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
                    can_delegate: false,
                    max_delegation_depth: 0,
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
                    can_delegate: false,
                    max_delegation_depth: 0,
                },
            ],
            context: HashMap::new(),
            worktree_mode: WorktreeMode::Shared,
            budget: None,
            max_parallel: 0,
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
                        "You decompose the task into subtasks with acceptance criteria."
                            .to_string(),
                    ),
                    skills: vec![],
                    model_override: None,
                    mcp_servers: vec![],
                    can_delegate: false,
                    max_delegation_depth: 0,
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
                    can_delegate: false,
                    max_delegation_depth: 0,
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
                    can_delegate: false,
                    max_delegation_depth: 0,
                },
            ],
            context: HashMap::new(),
            worktree_mode: WorktreeMode::Isolated,
            budget: None,
            max_parallel: 0,
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
                    can_delegate: false,
                    max_delegation_depth: 0,
                },
                TeamMemberDef {
                    role: "reviewer".to_string(),
                    agent_id: None,
                    system_prompt: Some("Review carefully".to_string()),
                    skills: vec!["review-changes".to_string()],
                    model_override: Some("claude-3-opus".to_string()),
                    mcp_servers: vec!["github".to_string()],
                    can_delegate: false,
                    max_delegation_depth: 0,
                },
            ],
            context: HashMap::from([("project".to_string(), "test-project".to_string())]),
            worktree_mode: WorktreeMode::Isolated,
            budget: None,
            max_parallel: 0,
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
                can_delegate: false,
                max_delegation_depth: 0,
            },
            TeamMemberDef {
                role: "reviewer".to_string(),
                agent_id: None,
                system_prompt: Some("Be thorough".to_string()),
                skills: vec![],
                model_override: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
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
            let restored: WorktreeMode = serde_json::from_str(&format!("\"{bare}\"")).unwrap();
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

    // ─── T-2: Validation Tests ─────────────────────────────────────────────

    #[test]
    fn validate_team_empty_members() {
        let mut team = test_team();
        team.members.clear();
        let err = validate_team(&team).unwrap_err();
        assert!(err.contains(&TeamValidationError::EmptyMembers));
    }

    #[test]
    fn validate_team_adversarial_wrong_count() {
        let mut team = test_team();
        team.coordination = TeamCoordination::Adversarial {
            max_rounds: 3,
            threshold: 0.8,
        };
        // test_team has 2 members, add a third
        team.members.push(TeamMemberDef {
            role: "observer".to_string(),
            agent_id: None,
            system_prompt: None,
            skills: vec![],
            model_override: None,
            mcp_servers: vec![],
            can_delegate: false,
            max_delegation_depth: 0,
        });
        let err = validate_team(&team).unwrap_err();
        assert!(
            err.iter()
                .any(|e| matches!(e, TeamValidationError::AdversarialMemberCount(3)))
        );
    }

    #[test]
    fn validate_team_adversarial_exact_two_ok() {
        let mut team = test_team();
        team.coordination = TeamCoordination::Adversarial {
            max_rounds: 3,
            threshold: 0.8,
        };
        assert!(validate_team(&team).is_ok());
    }

    #[test]
    fn validate_team_duplicate_roles() {
        let mut team = test_team();
        // Make both members have the same role
        team.members[1].role = team.members[0].role.clone();
        let err = validate_team(&team).unwrap_err();
        assert!(
            err.iter()
                .any(|e| matches!(e, TeamValidationError::DuplicateRoles(_)))
        );
    }

    #[test]
    fn validate_team_duplicate_agent_ids() {
        let mut team = test_team();
        team.members[0].agent_id = Some("same-id".to_string());
        team.members[1].agent_id = Some("same-id".to_string());
        let err = validate_team(&team).unwrap_err();
        assert!(
            err.iter()
                .any(|e| matches!(e, TeamValidationError::DuplicateAgentIds(_)))
        );
    }

    #[test]
    fn validate_team_valid_pipeline() {
        let team = test_team(); // pipeline with 2 distinct members
        assert!(validate_team(&team).is_ok());
    }

    #[test]
    fn validate_team_negative_budget_rejected() {
        let mut team = test_team();
        team.budget = Some(TeamBudget {
            max_cost_usd: -1.0,
            max_tokens: 100_000,
            max_duration_secs: 60,
        });
        let errs = validate_team(&team).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, TeamValidationError::InvalidBudget(_)))
        );
    }

    #[test]
    fn validate_team_all_zero_budget_rejected() {
        let mut team = test_team();
        team.budget = Some(TeamBudget {
            max_cost_usd: 0.0,
            max_tokens: 0,
            max_duration_secs: 0,
        });
        let errs = validate_team(&team).unwrap_err();
        assert!(errs.iter().any(|e| {
            matches!(e, TeamValidationError::InvalidBudget(msg) if msg.contains("all limits are zero"))
        }));
    }

    #[test]
    fn validate_team_valid_budget_accepted() {
        let mut team = test_team();
        team.budget = Some(TeamBudget {
            max_cost_usd: 5.0,
            max_tokens: 100_000,
            max_duration_secs: 300,
        });
        assert!(validate_team(&team).is_ok());
    }

    #[test]
    fn budget_serde_roundtrip() {
        let budget = TeamBudget {
            max_cost_usd: 10.0,
            max_tokens: 500_000,
            max_duration_secs: 600,
        };
        let json = serde_json::to_string(&budget).unwrap();
        let back: TeamBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(budget, back);
    }

    #[test]
    fn team_definition_serde_with_budget() {
        let mut team = test_team();
        team.budget = Some(TeamBudget {
            max_cost_usd: 2.5,
            max_tokens: 50_000,
            max_duration_secs: 120,
        });
        team.max_parallel = 3;
        let json = serde_json::to_string(&team).unwrap();
        let back: TeamDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_parallel, 3);
        assert_eq!(back.budget.unwrap().max_cost_usd, 2.5);
    }

    #[test]
    fn team_definition_serde_without_budget() {
        let team = test_team();
        let json = serde_json::to_string(&team).unwrap();
        assert!(!json.contains("budget"));
        let back: TeamDefinition = serde_json::from_str(&json).unwrap();
        assert!(back.budget.is_none());
        assert_eq!(back.max_parallel, 0);
    }

    // ─── T-2: Resolve with Registry Tests ──────────────────────────────────

    #[test]
    fn resolve_member_uses_registry_base() {
        let team = test_team();
        let member = &team.members[0]; // role = "coder", agent_id = "coder-agent"

        let mut registry = AgentProfileRegistry::new();
        let mut base = AgentProfile::new("coder-agent", "coder", AgentTier::System);
        base.system_prompt = Some("I am the registered coder.".to_string());
        base.skill_filter = vec!["search".to_string(), "read".to_string()];
        base.model_override = Some("gpt-4".to_string());
        registry.register(base).unwrap();

        let profile = resolve_member_to_profile_with_registry(member, &team, Some(&registry));

        // Member has no system_prompt override → registry prompt used
        assert_eq!(
            profile.system_prompt.as_deref(),
            Some("I am the registered coder.")
        );
        // Member has skills=["edit"] → overrides registry
        assert_eq!(profile.skill_filter, vec!["edit"]);
        // Member has no model_override → registry model preserved
        assert_eq!(profile.model_override.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn resolve_member_overrides_registry_with_member_values() {
        let team = test_team();
        let mut member = team.members[1].clone(); // reviewer, no agent_id → "team-test-team-reviewer"
        member.system_prompt = Some("Custom override prompt.".to_string());
        member.skills = vec!["edit".to_string()];
        member.model_override = Some("claude-4".to_string());

        let mut registry = AgentProfileRegistry::new();
        let mut base = AgentProfile::new("team-test-team-reviewer", "reviewer", AgentTier::System);
        base.system_prompt = Some("Registry prompt.".to_string());
        base.skill_filter = vec!["search".to_string()];
        base.model_override = Some("gpt-4".to_string());
        registry.register(base).unwrap();

        let profile = resolve_member_to_profile_with_registry(&member, &team, Some(&registry));

        // Member overrides win
        assert_eq!(
            profile.system_prompt.as_deref(),
            Some("Custom override prompt.")
        );
        assert_eq!(profile.skill_filter, vec!["edit"]);
        assert_eq!(profile.model_override.as_deref(), Some("claude-4"));
    }

    #[test]
    fn resolve_member_no_registry_generates_auto_id() {
        let team = test_team();
        let member = &team.members[1]; // reviewer, agent_id=None
        let profile = resolve_member_to_profile_with_registry(member, &team, None);
        assert_eq!(profile.agent_id, "team-test-team-reviewer");
    }

    #[test]
    fn resolve_member_no_registry_creates_fresh_profile() {
        let team = test_team();
        let member = &team.members[1]; // reviewer, agent_id=None
        let profile = resolve_member_to_profile_with_registry(member, &team, None);

        // member[1] has system_prompt = Some("Review carefully") → used as-is
        assert_eq!(profile.system_prompt.as_deref(), Some("Review carefully"));
        // Default tier is User (can_delegate = false)
        assert_eq!(profile.tier, AgentTier::User);
    }

    // ─── T-2: can_delegate / tier propagation ──────────────────────────────

    #[test]
    fn can_delegate_true_sets_system_tier() {
        let team = test_team();
        let mut member = team.members[0].clone();
        member.can_delegate = true;
        member.max_delegation_depth = 2;

        let profile = resolve_member_to_profile_with_registry(&member, &team, None);
        assert_eq!(profile.tier, AgentTier::System);
        assert!(profile.can_delegate);
        assert_eq!(profile.max_delegation_depth, 2);
    }

    #[test]
    fn can_delegate_false_keeps_user_tier() {
        let team = test_team();
        let member = &team.members[0]; // can_delegate = false
        let profile = resolve_member_to_profile_with_registry(member, &team, None);
        assert_eq!(profile.tier, AgentTier::User);
        assert!(!profile.can_delegate);
    }

    #[test]
    fn can_delegate_overrides_registry_tier() {
        let team = test_team();
        let mut member = team.members[0].clone(); // coder-agent
        member.can_delegate = true;

        let mut registry = AgentProfileRegistry::new();
        let base = AgentProfile::new("coder-agent", "coder", AgentTier::User);
        registry.register(base).unwrap();

        let profile = resolve_member_to_profile_with_registry(&member, &team, Some(&registry));
        assert_eq!(profile.tier, AgentTier::System);
        assert!(profile.can_delegate);
    }

    // ─── T-2: Metadata injection ───────────────────────────────────────────

    #[test]
    fn metadata_includes_team_name_and_role() {
        let team = test_team();
        let member = &team.members[0]; // coder
        let profile = resolve_member_to_profile_with_registry(member, &team, None);

        assert_eq!(
            profile.metadata.get("team_name"),
            Some(&serde_json::Value::String("test-team".to_string()))
        );
        assert_eq!(
            profile.metadata.get("team_role"),
            Some(&serde_json::Value::String("coder".to_string()))
        );
    }

    #[test]
    fn metadata_includes_team_context() {
        let team = test_team(); // has context = {"project": "test-project"}
        let member = &team.members[0];
        let profile = resolve_member_to_profile_with_registry(member, &team, None);

        let ctx = profile.metadata.get("team_context").unwrap();
        let ctx_map: HashMap<String, String> = serde_json::from_value(ctx.clone()).unwrap();
        assert_eq!(ctx_map.get("project"), Some(&"test-project".to_string()));
    }

    #[test]
    fn metadata_empty_context_no_team_context_key() {
        let mut team = test_team();
        team.context.clear();
        let member = &team.members[0];
        let profile = resolve_member_to_profile_with_registry(member, &team, None);

        // team_name and team_role always present
        assert!(profile.metadata.contains_key("team_name"));
        assert!(profile.metadata.contains_key("team_role"));
        // team_context absent when context is empty
        assert!(!profile.metadata.contains_key("team_context"));
    }

    // ─── T-2: resolve_team bulk tests ──────────────────────────────────────

    #[test]
    fn resolve_team_returns_profiles_and_request() {
        let team = test_team();
        let (request, profiles) = resolve_team(&team, "Fix auth", "run-1", None).unwrap();

        assert_eq!(profiles.len(), 2);
        assert_eq!(request.task, "Fix auth");
        assert_eq!(request.parent_run_id, "run-1");
        assert_eq!(request.user_id, team.user_id);
        assert_eq!(request.depth, 0);
    }

    #[test]
    fn resolve_team_with_registry() {
        let team = test_team();
        let mut registry = AgentProfileRegistry::new();
        let mut base = AgentProfile::new("coder-agent", "coder", AgentTier::System);
        base.system_prompt = Some("Registered coder prompt.".to_string());
        registry.register(base).unwrap();

        let (_request, profiles) = resolve_team(&team, "task", "run-1", Some(&registry)).unwrap();

        // First profile (coder-agent) should use registry base prompt
        assert_eq!(
            profiles[0].system_prompt.as_deref(),
            Some("Registered coder prompt.")
        );
        // Second profile (reviewer) not in registry → uses member's system_prompt
        assert_eq!(
            profiles[1].system_prompt.as_deref(),
            Some("Review carefully")
        );
    }

    #[test]
    fn resolve_team_rejects_invalid() {
        let mut team = test_team();
        team.members.clear();
        let result = resolve_team(&team, "task", "run-1", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("at least one member"));
    }

    #[test]
    fn resolve_team_context_propagated_to_request() {
        let team = test_team();
        let (request, _) = resolve_team(&team, "task", "run-1", None).unwrap();
        assert_eq!(
            request.context.get("project"),
            Some(&serde_json::Value::String("test-project".to_string()))
        );
    }

    // ─── T-2: can_delegate / max_delegation_depth serde ────────────────────

    #[test]
    fn team_member_def_serde_defaults() {
        // Deserialize without can_delegate/max_delegation_depth → defaults
        let json = r#"{
            "role": "worker",
            "agent_id": null,
            "system_prompt": null,
            "skills": [],
            "model_override": null,
            "mcp_servers": []
        }"#;
        let member: TeamMemberDef = serde_json::from_str(json).unwrap();
        assert!(!member.can_delegate);
        assert_eq!(member.max_delegation_depth, 0);
    }

    #[test]
    fn team_member_def_serde_with_delegation() {
        let json = r#"{
            "role": "orchestrator",
            "agent_id": "orch-1",
            "system_prompt": "Run the show",
            "skills": ["delegate"],
            "model_override": null,
            "mcp_servers": [],
            "can_delegate": true,
            "max_delegation_depth": 3
        }"#;
        let member: TeamMemberDef = serde_json::from_str(json).unwrap();
        assert!(member.can_delegate);
        assert_eq!(member.max_delegation_depth, 3);
    }

    // ─── T-2: build_coordination_pattern via resolve_team ──────────────────

    #[test]
    fn resolve_team_pipeline_pattern_stages() {
        let team = test_team(); // Pipeline coordination
        let (request, _) = resolve_team(&team, "task", "run-1", None).unwrap();
        match &request.pattern {
            CoordinationPattern::Pipeline { stages } => {
                assert_eq!(stages.len(), 2);
                assert_eq!(stages[0].agent_id, "coder-agent");
                assert_eq!(stages[1].agent_id, "team-test-team-reviewer");
            }
            _ => panic!("expected Pipeline pattern"),
        }
    }

    #[test]
    fn resolve_team_adversarial_pattern() {
        let mut team = test_team();
        team.coordination = TeamCoordination::Adversarial {
            max_rounds: 5,
            threshold: 0.9,
        };
        let (request, _) = resolve_team(&team, "task", "run-1", None).unwrap();
        match &request.pattern {
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
            _ => panic!("expected AdversarialReview pattern"),
        }
    }

    #[test]
    fn resolve_team_fan_out_pattern() {
        let mut team = test_team();
        team.coordination = TeamCoordination::FanOut {
            aggregation: "best_of".to_string(),
        };
        let (request, _) = resolve_team(&team, "task", "run-1", None).unwrap();
        match &request.pattern {
            CoordinationPattern::FanOut { agent_ids, .. } => {
                assert_eq!(agent_ids.len(), 2);
            }
            _ => panic!("expected FanOut pattern"),
        }
    }

    // ── Execution Recording ──

    #[tokio::test]
    async fn in_memory_store_execution_recording() {
        let store = InMemoryTeamStore::new();

        // Record start
        store
            .record_execution_start("exec-1", "team-1", "user-1", "build the app")
            .await
            .unwrap();

        // List should show 1 running execution
        let list = store.list_executions("team-1", 10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].execution_id, "exec-1");
        assert_eq!(list[0].task, "build the app");
        assert_eq!(list[0].status, "running");

        // Complete it
        store
            .record_execution_complete("exec-1", "completed", Some(r#"{"ok":true}"#))
            .await
            .unwrap();

        let list = store.list_executions("team-1", 10).await.unwrap();
        assert_eq!(list[0].status, "completed");
        assert!(list[0].result_json.is_some());
    }

    #[tokio::test]
    async fn in_memory_store_execution_list_limit() {
        let store = InMemoryTeamStore::new();
        for i in 0..5 {
            store
                .record_execution_start(
                    &format!("exec-{i}"),
                    "team-1",
                    "user-1",
                    &format!("task {i}"),
                )
                .await
                .unwrap();
        }
        let list = store.list_executions("team-1", 3).await.unwrap();
        assert_eq!(list.len(), 3);
    }

    #[tokio::test]
    async fn in_memory_store_execution_complete_unknown_id() {
        let store = InMemoryTeamStore::new();
        // Completing a non-existent execution should succeed silently (no-op)
        let result = store
            .record_execution_complete("nonexistent", "completed", None)
            .await;
        assert!(result.is_ok());
    }

    // ── Execution Retention ──

    #[tokio::test]
    async fn execution_retention_prunes_oldest_when_limit_exceeded() {
        let store = InMemoryTeamStore::new();
        // Fill to exactly MAX (100) and complete them so they're eligible for pruning
        for i in 0..100 {
            store
                .record_execution_start(
                    &format!("exec-{i}"),
                    "team-ret",
                    "user-1",
                    &format!("task {i}"),
                )
                .await
                .unwrap();
            store
                .record_execution_complete(&format!("exec-{i}"), "completed", None)
                .await
                .unwrap();
        }
        let all = store.list_executions("team-ret", 200).await.unwrap();
        assert_eq!(all.len(), 100);

        // Adding one more triggers prune of oldest completed (exec-0)
        store
            .record_execution_start("exec-100", "team-ret", "user-1", "task 100")
            .await
            .unwrap();

        let all = store.list_executions("team-ret", 200).await.unwrap();
        // 100 completed - 1 pruned + 1 new running = 100
        assert_eq!(all.len(), 100);
        assert!(
            all.iter().all(|r| r.execution_id != "exec-0"),
            "oldest completed execution should have been pruned"
        );
        assert!(all.iter().any(|r| r.execution_id == "exec-1"));
        assert!(all.iter().any(|r| r.execution_id == "exec-100"));
    }

    #[tokio::test]
    async fn execution_retention_does_not_prune_other_teams() {
        let store = InMemoryTeamStore::new();
        // Fill team-a to 100 completed
        for i in 0..100 {
            store
                .record_execution_start(
                    &format!("a-exec-{i}"),
                    "team-a",
                    "user-1",
                    &format!("task {i}"),
                )
                .await
                .unwrap();
            store
                .record_execution_complete(&format!("a-exec-{i}"), "completed", None)
                .await
                .unwrap();
        }
        // Add 3 for team-b
        for i in 0..3 {
            store
                .record_execution_start(
                    &format!("b-exec-{i}"),
                    "team-b",
                    "user-1",
                    &format!("task {i}"),
                )
                .await
                .unwrap();
        }

        // Trigger prune on team-a
        store
            .record_execution_start("a-exec-100", "team-a", "user-1", "overflow")
            .await
            .unwrap();

        // team-b should be untouched
        let b_list = store.list_executions("team-b", 200).await.unwrap();
        assert_eq!(b_list.len(), 3);

        // team-a should be capped at 100
        let a_list = store.list_executions("team-a", 200).await.unwrap();
        assert_eq!(a_list.len(), 100);
    }

    #[tokio::test]
    async fn execution_retention_preserves_running_records() {
        let store = InMemoryTeamStore::new();
        // Create 100 running (not completed) records
        for i in 0..100 {
            store
                .record_execution_start(
                    &format!("exec-{i}"),
                    "team-run",
                    "user-1",
                    &format!("task {i}"),
                )
                .await
                .unwrap();
        }
        // Add one more — running records should NOT be pruned
        store
            .record_execution_start("exec-100", "team-run", "user-1", "task 100")
            .await
            .unwrap();

        let all = store.list_executions("team-run", 200).await.unwrap();
        assert_eq!(all.len(), 101, "running records must not be pruned");
    }

    // ── Snapshot CRUD ──

    #[tokio::test]
    async fn in_memory_store_snapshot_crud() {
        let store = InMemoryTeamStore::new();

        let snap = TeamSnapshotRecord {
            snapshot_id: "snap-1".to_string(),
            team_name: "team-a".to_string(),
            user_id: "user-1".to_string(),
            label: "before refactor".to_string(),
            git_commit: Some("abc123".to_string()),
            session_id: Some("sess-1".to_string()),
            team_definition_json: Some(r#"{"name":"team-a"}"#.to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };

        // Save
        store.save_snapshot(&snap).await.unwrap();

        // List
        let list = store.list_snapshots("team-a", "user-1", 50).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].label, "before refactor");

        // Find by exact ID
        let found = store.find_snapshot("snap-1", "user-1").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().git_commit, Some("abc123".to_string()));

        // Delete
        let deleted = store.delete_snapshot("snap-1", "user-1").await.unwrap();
        assert!(deleted);

        // Verify gone
        let gone = store.find_snapshot("snap-1", "user-1").await.unwrap();
        assert!(gone.is_none());
    }

    #[tokio::test]
    async fn in_memory_store_snapshot_list_by_team() {
        let store = InMemoryTeamStore::new();

        for (id, team) in [("s1", "team-a"), ("s2", "team-a"), ("s3", "team-b")] {
            store
                .save_snapshot(&TeamSnapshotRecord {
                    snapshot_id: id.to_string(),
                    team_name: team.to_string(),
                    user_id: "u1".to_string(),
                    label: format!("snap {id}"),
                    git_commit: None,
                    session_id: None,
                    team_definition_json: None,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                })
                .await
                .unwrap();
        }

        let a_snaps = store.list_snapshots("team-a", "u1", 50).await.unwrap();
        assert_eq!(a_snaps.len(), 2);

        let b_snaps = store.list_snapshots("team-b", "u1", 50).await.unwrap();
        assert_eq!(b_snaps.len(), 1);
    }

    #[tokio::test]
    async fn in_memory_store_snapshot_find_not_found() {
        let store = InMemoryTeamStore::new();
        let result = store.find_snapshot("nonexistent", "u1").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn in_memory_store_snapshot_scoped_by_user_id() {
        let store = InMemoryTeamStore::new();
        let snap = TeamSnapshotRecord {
            snapshot_id: "snap-shared-id".to_string(),
            team_name: "team-x".to_string(),
            user_id: "alice".to_string(),
            label: "alice snap".to_string(),
            git_commit: None,
            session_id: None,
            team_definition_json: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        store.save_snapshot(&snap).await.unwrap();

        assert!(
            store
                .find_snapshot("snap-shared-id", "bob")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .list_snapshots("team-x", "bob", 50)
                .await
                .unwrap()
                .is_empty()
        );

        let deleted = store
            .delete_snapshot("snap-shared-id", "bob")
            .await
            .unwrap();
        assert!(!deleted, "other user must not delete alice snapshot");

        assert!(
            store
                .find_snapshot("snap-shared-id", "alice")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn in_memory_store_snapshot_with_team_definition() {
        let store = InMemoryTeamStore::new();

        let def_json = serde_json::json!({
            "name": "test-team",
            "members": [{"role": "coder"}],
        })
        .to_string();

        store
            .save_snapshot(&TeamSnapshotRecord {
                snapshot_id: "snap-def".to_string(),
                team_name: "test-team".to_string(),
                user_id: "u1".to_string(),
                label: "with definition".to_string(),
                git_commit: None,
                session_id: None,
                team_definition_json: Some(def_json.clone()),
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await
            .unwrap();

        let found = store
            .find_snapshot("snap-def", "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.team_definition_json, Some(def_json));
    }
}
