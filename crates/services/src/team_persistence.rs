//! Team definitions persistence — MatrixOne-backed storage for team configurations.
//!
//! Provides a [`TeamPersistenceService`] trait for CRUD operations and an in-memory
//! implementation for CLI / test use. Database-backed implementation uses the
//! `team_definitions` table in MatrixOne.

use astra_core::is_duplicate_key_error;
use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::{MySql, QueryBuilder};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, RwLock};

use crate::coordination::{AgentProfile, AgentTier};

const MAX_TEAM_LIST_ROWS: usize = 200;
const MAX_TEAM_SNAPSHOT_LIST_ROWS: u32 = 200;
const BUILTIN_OWNER_INIT_CACHE_CAPACITY: usize = 4096;
const TEAM_LIST_SELECT_SQL: &str = "\
    SELECT team_id, user_id, name, description, \
           members_json, context_json, \
           CAST(created_at AS CHAR) AS created_at, \
           CAST(updated_at AS CHAR) AS updated_at \
    FROM team_definitions \
    WHERE user_id = ? \
    ORDER BY name \
    LIMIT ?";

fn validate_team_snapshot_list_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_TEAM_SNAPSHOT_LIST_ROWS)
}

fn team_list_query_limit(limit: u32) -> i64 {
    i64::from(limit) + 1
}

fn team_cursor_db_timestamp(
    label: &'static str,
    value: &str,
    context: &str,
) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("invalid {context} cursor: {label} is required"));
    }
    let mut db_value = trimmed.replace('T', " ");
    if let Some(stripped) = db_value.strip_suffix('Z') {
        db_value = stripped.to_string();
    }
    if chrono::NaiveDateTime::parse_from_str(&db_value, "%Y-%m-%d %H:%M:%S%.f").is_ok() {
        return Ok(db_value);
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.naive_utc().format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        .map_err(|_| format!("invalid {context} cursor timestamp: {value}"))
}

fn team_cursor_required_id(
    label: &'static str,
    value: &str,
    context: &str,
) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("invalid {context} cursor: {label} is required"));
    }
    Ok(trimmed.to_string())
}

// ─── Team Definition Types ──────────────────────────────────────────────────

/// Persistent team definition stored in MatrixOne.
///
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamDefinition {
    pub team_id: String,
    pub user_id: String,
    pub name: String,
    pub description: String,
    pub members: Vec<TeamMemberDef>,
    pub context: HashMap<String, String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Lightweight member declaration within a team.
///
/// Resolved to a full [`AgentProfile`] at execution time via [`resolve_member_to_profile`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamMemberDef {
    pub role: String,
    /// Logical identity inside this owner-scoped team. When `None`, an
    /// execution-private identity is derived from the team and role.
    pub agent_id: Option<String>,
    pub system_prompt: Option<String>,
    pub skills: Vec<String>,
    /// Optional execution-tool allowlist. `None` inherits the admitted tool
    /// surface; `Some(empty)` is an explicit deny-all.
    #[serde(default)]
    pub allow_tools: Option<Vec<String>>,
    /// Whether this member is restricted to read-only execution.
    #[serde(default)]
    pub read_only: bool,
    /// Optional initial adaptive turn slice.
    #[serde(default)]
    pub initial_turns: Option<u32>,
    /// Optional hard maximum turn budget.
    #[serde(default)]
    pub max_turns: Option<u32>,
    pub model_selection: Option<astra_turn_types::ModelSelection>,
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

// ─── Resolve: TeamMemberDef → AgentProfile ──────────────────────────────────

/// Convert a team member declaration into a full [`AgentProfile`].
///
/// The generated profile inherits the team description as context in its system
/// prompt and stores team context in `metadata["team_context"]`.
pub fn resolve_member_to_profile(member: &TeamMemberDef, team: &TeamDefinition) -> AgentProfile {
    let agent_id = member
        .agent_id
        .clone()
        .unwrap_or_else(|| format!("team-{}-{}", team.name, member.role));

    // Team definitions are the complete source of member authority. Never
    // inherit a process-global profile with the same naked agent_id: that
    // would let another tenant alter this execution's prompt, tools or tier.
    let tier = if member.can_delegate {
        AgentTier::System
    } else {
        AgentTier::User
    };
    let mut profile = AgentProfile::new(&agent_id, &member.role, tier);

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
    if member.model_selection.is_some() {
        profile.model_selection = member.model_selection.clone();
    }
    if !member.mcp_servers.is_empty() {
        profile.mcp_servers = member.mcp_servers.clone();
    }

    // Preserve the execution controls independently from the skill selector.
    profile.allow_tools = member.allow_tools.clone();
    profile.read_only = member.read_only;
    profile.initial_turns = member.initial_turns;
    profile.max_turns = member.max_turns;

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
    /// Duplicate role names within the same team.
    DuplicateRoles(Vec<String>),
    /// Duplicate agent IDs (explicit or generated).
    DuplicateAgentIds(Vec<String>),
    /// A member contains an invalid canonical profile control.
    InvalidMember(String),
}

impl std::fmt::Display for TeamValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateRoles(roles) => {
                write!(f, "duplicate roles: {}", roles.join(", "))
            }
            Self::DuplicateAgentIds(ids) => {
                write!(f, "duplicate agent IDs: {}", ids.join(", "))
            }
            Self::InvalidMember(msg) => {
                write!(f, "invalid team member: {msg}")
            }
        }
    }
}

/// Validate a team configuration, including an unfinished member roster.
///
/// Checks:
/// - No duplicate roles
/// - No duplicate agent IDs (after resolution)
pub fn validate_team(team: &TeamDefinition) -> Result<(), Vec<TeamValidationError>> {
    let mut errors = Vec::new();

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

        if m.agent_id
            .as_deref()
            .is_some_and(|agent_id| agent_id.trim().is_empty())
        {
            errors.push(TeamValidationError::InvalidMember(format!(
                "role '{}' agent_id must not be empty",
                m.role
            )));
        }

        if m.initial_turns == Some(0) {
            errors.push(TeamValidationError::InvalidMember(format!(
                "role '{}' initial_turns must be positive",
                m.role
            )));
        }
        if m.max_turns == Some(0) {
            errors.push(TeamValidationError::InvalidMember(format!(
                "role '{}' max_turns must be positive",
                m.role
            )));
        }
        if let (Some(initial), Some(maximum)) = (m.initial_turns, m.max_turns)
            && initial > maximum
        {
            errors.push(TeamValidationError::InvalidMember(format!(
                "role '{}' initial_turns cannot exceed max_turns",
                m.role
            )));
        }
    }
    let dup_ids: Vec<String> = id_counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(id, _)| id)
        .collect();
    if !dup_ids.is_empty() {
        errors.push(TeamValidationError::DuplicateAgentIds(dup_ids));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSnapshotListCursor {
    pub created_at: String,
    pub snapshot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamSnapshotListPage {
    pub snapshots: Vec<TeamSnapshotRecord>,
    pub limit: u32,
    pub next_cursor: Option<TeamSnapshotListCursor>,
}

pub fn team_snapshot_cursor_db_created_at(
    cursor: &TeamSnapshotListCursor,
) -> Result<String, String> {
    team_cursor_db_timestamp("created_at", &cursor.created_at, "team snapshot list")
}

pub fn team_snapshot_cursor_snapshot_id(cursor: &TeamSnapshotListCursor) -> Result<String, String> {
    team_cursor_required_id("snapshot_id", &cursor.snapshot_id, "team snapshot list")
}

fn sort_team_snapshots_recent(snapshots: &mut [TeamSnapshotRecord]) {
    snapshots.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.snapshot_id.cmp(&left.snapshot_id))
    });
}

fn team_snapshot_after_cursor(
    snapshot: &TeamSnapshotRecord,
    cursor: &TeamSnapshotListCursor,
) -> bool {
    snapshot.created_at < cursor.created_at
        || (snapshot.created_at == cursor.created_at && snapshot.snapshot_id < cursor.snapshot_id)
}

fn team_snapshot_cursor_from_record(
    snapshot: &TeamSnapshotRecord,
) -> Result<TeamSnapshotListCursor, String> {
    if snapshot.created_at.trim().is_empty() {
        return Err(format!(
            "invalid team_snapshots cursor: snapshot_id={}, column=created_at, value is empty",
            snapshot.snapshot_id
        ));
    }
    if snapshot.snapshot_id.trim().is_empty() {
        return Err(
            "invalid team_snapshots cursor: column=snapshot_id, value is empty".to_string(),
        );
    }
    Ok(TeamSnapshotListCursor {
        created_at: snapshot.created_at.clone(),
        snapshot_id: snapshot.snapshot_id.clone(),
    })
}

fn team_snapshot_page_from_records(
    mut snapshots: Vec<TeamSnapshotRecord>,
    limit: u32,
    cursor: Option<TeamSnapshotListCursor>,
) -> Result<TeamSnapshotListPage, String> {
    let limit = validate_team_snapshot_list_limit(limit);
    sort_team_snapshots_recent(&mut snapshots);
    if let Some(cursor) = &cursor {
        team_snapshot_cursor_db_created_at(cursor)?;
        team_snapshot_cursor_snapshot_id(cursor)?;
        snapshots.retain(|snapshot| team_snapshot_after_cursor(snapshot, cursor));
    }
    let has_more = snapshots.len() > limit as usize;
    if has_more {
        snapshots.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        snapshots
            .last()
            .map(team_snapshot_cursor_from_record)
            .transpose()?
    } else {
        None
    };
    Ok(TeamSnapshotListPage {
        snapshots,
        limit,
        next_cursor,
    })
}

// ─── Persistence Trait ──────────────────────────────────────────────────────

/// CRUD operations for team definitions and snapshots.
#[async_trait]
pub trait TeamPersistenceService: Send + Sync {
    /// Materialize the standard team templates for an authenticated owner.
    ///
    /// This is intentionally request-owner scoped. Process startup has no
    /// legitimate user identity and must never seed templates under a fake
    /// principal.
    async fn ensure_builtins(&self, user_id: &str) -> Result<(), String>;

    async fn save_team(&self, team: &TeamDefinition) -> Result<TeamDefinition, String>;
    async fn load_team(&self, user_id: &str, name: &str) -> Result<Option<TeamDefinition>, String>;
    async fn load_team_by_id(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Option<TeamDefinition>, String>;
    async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamDefinition>, String>;
    async fn delete_team(&self, user_id: &str, name: &str) -> Result<bool, String>;

    // ── Snapshots ───────────────────────────────────────────────

    /// Save a team snapshot and return its accepted identity and contents.
    async fn save_snapshot(
        &self,
        snapshot: &TeamSnapshotRecord,
    ) -> Result<TeamSnapshotRecord, String>;

    /// List snapshots for a team, most recent first. Default: empty.
    async fn list_snapshots(
        &self,
        _team_name: &str,
        _user_id: &str,
        _limit: u32,
    ) -> Result<Vec<TeamSnapshotRecord>, String> {
        Ok(vec![])
    }

    async fn list_snapshots_page(
        &self,
        team_name: &str,
        user_id: &str,
        limit: u32,
        cursor: Option<TeamSnapshotListCursor>,
    ) -> Result<TeamSnapshotListPage, String> {
        let snapshots = self
            .list_snapshots(team_name, user_id, MAX_TEAM_SNAPSHOT_LIST_ROWS)
            .await?;
        team_snapshot_page_from_records(snapshots, limit, cursor)
    }

    /// Find an owner-scoped snapshot by its complete, exact ID.
    async fn find_snapshot(
        &self,
        snapshot_id: &str,
        user_id: &str,
    ) -> Result<Option<TeamSnapshotRecord>, String>;

    /// Delete a snapshot by ID. Returns true if found and deleted.
    async fn delete_snapshot(&self, _snapshot_id: &str, _user_id: &str) -> Result<bool, String> {
        Ok(false)
    }
}

// ─── In-Memory Implementation ───────────────────────────────────────────────

/// In-memory implementation suitable for CLI use and testing.
pub struct InMemoryTeamStore {
    teams: RwLock<HashMap<String, TeamDefinition>>,
    snapshots: RwLock<Vec<TeamSnapshotRecord>>,
}

impl InMemoryTeamStore {
    pub fn new() -> Self {
        Self {
            teams: RwLock::new(HashMap::new()),
            snapshots: RwLock::new(Vec::new()),
        }
    }

    /// Create a store pre-populated with the three built-in teams.
    pub fn with_builtins(user_id: &str) -> Self {
        let store = Self::new();
        let now = chrono::Utc::now().to_rfc3339();
        let builtins = builtin_teams(user_id, &now);
        {
            let mut map = astra_core::sync_poison::recover_rwlock_write(&store.teams);
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
    async fn ensure_builtins(&self, user_id: &str) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut map = self.teams.write().map_err(|e| e.to_string())?;
        for team in builtin_teams(user_id, &now) {
            let key = format!("{}:{}", team.user_id, team.name);
            map.entry(key).or_insert(team);
        }
        Ok(())
    }

    async fn save_team(&self, team: &TeamDefinition) -> Result<TeamDefinition, String> {
        let key = format!("{}:{}", team.user_id, team.name);
        let mut map = self.teams.write().map_err(|e| e.to_string())?;
        map.insert(key, team.clone());
        Ok(team.clone())
    }

    async fn load_team(&self, user_id: &str, name: &str) -> Result<Option<TeamDefinition>, String> {
        let key = format!("{user_id}:{name}");
        let map = self.teams.read().map_err(|e| e.to_string())?;
        Ok(map.get(&key).cloned())
    }

    async fn load_team_by_id(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Option<TeamDefinition>, String> {
        let map = self.teams.read().map_err(|e| e.to_string())?;
        Ok(map
            .values()
            .find(|team| team.user_id == user_id && team.team_id == team_id)
            .cloned())
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
        teams.truncate(MAX_TEAM_LIST_ROWS);
        Ok(teams)
    }

    async fn delete_team(&self, user_id: &str, name: &str) -> Result<bool, String> {
        let key = format!("{user_id}:{name}");
        let mut map = self.teams.write().map_err(|e| e.to_string())?;
        Ok(map.remove(&key).is_some())
    }

    // ── Snapshots ───────────────────────────────────────────────

    async fn save_snapshot(
        &self,
        snapshot: &TeamSnapshotRecord,
    ) -> Result<TeamSnapshotRecord, String> {
        let mut snaps = self.snapshots.write().map_err(|e| e.to_string())?;
        snaps.push(snapshot.clone());
        Ok(snapshot.clone())
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
        sort_team_snapshots_recent(&mut matching);
        matching.truncate(limit as usize);
        Ok(matching)
    }

    async fn list_snapshots_page(
        &self,
        team_name: &str,
        user_id: &str,
        limit: u32,
        cursor: Option<TeamSnapshotListCursor>,
    ) -> Result<TeamSnapshotListPage, String> {
        let snaps = self.snapshots.read().map_err(|e| e.to_string())?;
        let matching: Vec<_> = snaps
            .iter()
            .filter(|s| s.team_name == team_name && s.user_id == user_id)
            .cloned()
            .collect();
        team_snapshot_page_from_records(matching, limit, cursor)
    }

    async fn find_snapshot(
        &self,
        snapshot_id: &str,
        user_id: &str,
    ) -> Result<Option<TeamSnapshotRecord>, String> {
        let snaps = self.snapshots.read().map_err(|e| e.to_string())?;
        Ok(snaps
            .iter()
            .find(|s| s.snapshot_id == snapshot_id && s.user_id == user_id)
            .cloned())
    }

    async fn delete_snapshot(&self, snapshot_id: &str, user_id: &str) -> Result<bool, String> {
        let mut snaps = self.snapshots.write().map_err(|e| e.to_string())?;
        let before = snaps.len();
        snaps.retain(|s| !(s.snapshot_id == snapshot_id && s.user_id == user_id));
        Ok(snaps.len() < before)
    }
}

// ─── MatrixOne-backed Implementation ────────────────────────────────────────

struct OwnerInitializationCache {
    capacity: usize,
    inner: tokio::sync::Mutex<OwnerInitializationCacheInner>,
}

#[derive(Default)]
struct OwnerInitializationCacheInner {
    entries: HashMap<String, Arc<tokio::sync::OnceCell<()>>>,
    order: VecDeque<String>,
}

impl OwnerInitializationCache {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "owner initialization cache must be bounded");
        Self {
            capacity,
            inner: tokio::sync::Mutex::new(OwnerInitializationCacheInner::default()),
        }
    }

    async fn cell_for(&self, user_id: &str) -> Arc<tokio::sync::OnceCell<()>> {
        let mut inner = self.inner.lock().await;
        if let Some(cell) = inner.entries.get(user_id).cloned() {
            inner.order.retain(|owner| owner != user_id);
            inner.order.push_back(user_id.to_string());
            return cell;
        }

        while inner.entries.len() >= self.capacity {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            inner.entries.remove(&oldest);
        }
        let cell = Arc::new(tokio::sync::OnceCell::new());
        inner.entries.insert(user_id.to_string(), Arc::clone(&cell));
        inner.order.push_back(user_id.to_string());
        cell
    }

    async fn ensure<F, Fut>(&self, user_id: &str, initialize: F) -> Result<(), String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), String>>,
    {
        self.cell_for(user_id)
            .await
            .get_or_try_init(|| async { initialize().await })
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.inner.lock().await.entries.len()
    }
}

/// Team persistence backed by MatrixOne's `team_definitions` table.
///
/// Uses sqlx connection pool with parameterized queries. The schema is created
/// by [`crate::storage::ensure_core_schema`].
///
/// Member declarations and shared context are stored as JSON text columns.
pub struct MatrixOneTeamStore {
    pool: sqlx::Pool<sqlx::MySql>,
    owner_initializations: OwnerInitializationCache,
}

impl MatrixOneTeamStore {
    /// Create from an existing connection pool.
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self {
            pool,
            owner_initializations: OwnerInitializationCache::new(BUILTIN_OWNER_INIT_CACHE_CAPACITY),
        }
    }

    /// Materialize built-in teams without overwriting an owner's existing
    /// same-name definition.
    ///
    /// This uses insert-only admission rather than `load` followed by
    /// [`TeamPersistenceService::save_team`]. The latter is an upsert and can
    /// overwrite a user customization when first-request initialization races
    /// with an explicit create.
    pub async fn ensure_builtins(&self, user_id: &str) -> Result<(), String> {
        self.owner_initializations
            .ensure(user_id, || self.ensure_builtins_uncached(user_id))
            .await
    }

    async fn ensure_builtins_uncached(&self, user_id: &str) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        for team in builtin_teams(user_id, &now) {
            self.insert_builtin_if_absent(&team).await?;
        }
        Ok(())
    }

    async fn insert_builtin_if_absent(&self, team: &TeamDefinition) -> Result<(), String> {
        let members_json = serde_json::to_string(&team.members).map_err(|e| e.to_string())?;
        let context_json = serde_json::to_string(&team.context).map_err(|e| e.to_string())?;

        match sqlx::query(
            "INSERT INTO team_definitions \
             (team_id, user_id, name, description, members_json, \
              context_json, \
              created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(&team.team_id)
        .bind(&team.user_id)
        .bind(&team.name)
        .bind(&team.description)
        .bind(&members_json)
        .bind(&context_json)
        .execute(&self.pool)
        .await
        {
            Ok(_) => Ok(()),
            Err(error) if is_duplicate_key_error(&error) => {
                if self.load_team(&team.user_id, &team.name).await?.is_some() {
                    Ok(())
                } else {
                    Err(format!(
                        "builtin team INSERT failed: team_id {} collides with a different team",
                        team.team_id
                    ))
                }
            }
            Err(error) => Err(format!("builtin team INSERT failed: {error}")),
        }
    }
}

#[async_trait]
impl TeamPersistenceService for MatrixOneTeamStore {
    async fn ensure_builtins(&self, user_id: &str) -> Result<(), String> {
        MatrixOneTeamStore::ensure_builtins(self, user_id).await
    }

    async fn save_team(&self, team: &TeamDefinition) -> Result<TeamDefinition, String> {
        let members_json = serde_json::to_string(&team.members).map_err(|e| e.to_string())?;
        let context_json = serde_json::to_string(&team.context).map_err(|e| e.to_string())?;

        // Insert first so concurrent creates collapse into a duplicate-key path, then
        // update only the logical team identified by UNIQUE(user_id, name).
        match sqlx::query(
            "INSERT INTO team_definitions \
             (team_id, user_id, name, description, members_json, \
              context_json, \
              created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(&team.team_id)
        .bind(&team.user_id)
        .bind(&team.name)
        .bind(&team.description)
        .bind(&members_json)
        .bind(&context_json)
        .execute(&self.pool)
        .await
        {
            Ok(_) => {}
            Err(error) if is_duplicate_key_error(&error) => {
                let updated = sqlx::query(
                    "UPDATE team_definitions SET \
                         team_id       = ?, \
                         description   = ?, \
                         members_json  = ?, \
                         context_json  = ?, \
                         updated_at    = NOW(6) \
                     WHERE user_id = ? AND name = ?",
                )
                .bind(&team.team_id)
                .bind(&team.description)
                .bind(&members_json)
                .bind(&context_json)
                .bind(&team.user_id)
                .bind(&team.name)
                .execute(&self.pool)
                .await
                .map_err(|e| format!("team UPDATE after duplicate failed: {e}"))?;

                if updated.rows_affected() == 0 {
                    return Err(format!(
                        "team INSERT failed: duplicate team_id {} belongs to a different team",
                        team.team_id
                    ));
                }
            }
            Err(error) => return Err(format!("team INSERT failed: {error}")),
        }

        Ok(team.clone())
    }

    async fn load_team(&self, user_id: &str, name: &str) -> Result<Option<TeamDefinition>, String> {
        let row = sqlx::query(
            "SELECT team_id, user_id, name, description, \
                    members_json, context_json, \
                    CAST(created_at AS CHAR) AS created_at, \
                    CAST(updated_at AS CHAR) AS updated_at \
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

    async fn load_team_by_id(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Option<TeamDefinition>, String> {
        let row = sqlx::query(
            "SELECT team_id, user_id, name, description, \
                    members_json, context_json, \
                    CAST(created_at AS CHAR) AS created_at, \
                    CAST(updated_at AS CHAR) AS updated_at \
             FROM team_definitions WHERE user_id = ? AND team_id = ?",
        )
        .bind(user_id)
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("team SELECT by id failed: {e}"))?;

        match row {
            None => Ok(None),
            Some(row) => {
                let team = row_to_team_definition(&row)?;
                Ok(Some(team))
            }
        }
    }

    async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamDefinition>, String> {
        let rows = sqlx::query(TEAM_LIST_SELECT_SQL)
            .bind(user_id)
            .bind(MAX_TEAM_LIST_ROWS as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| format!("team list failed: {e}"))?;

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

    // ── Snapshots ───────────────────────────────────────────────

    async fn save_snapshot(
        &self,
        snapshot: &TeamSnapshotRecord,
    ) -> Result<TeamSnapshotRecord, String> {
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
        Ok(snapshot.clone())
    }

    async fn list_snapshots(
        &self,
        team_name: &str,
        user_id: &str,
        limit: u32,
    ) -> Result<Vec<TeamSnapshotRecord>, String> {
        let rows = sqlx::query(
            "SELECT snapshot_id, team_name, user_id, label, git_commit, \
                    session_id, team_definition_json, \
                    CAST(created_at AS CHAR) AS created_at \
             FROM team_snapshots \
             WHERE user_id = ? AND team_name = ? \
             ORDER BY created_at DESC, snapshot_id DESC \
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
            records.push(row_to_team_snapshot_record(row)?);
        }
        Ok(records)
    }

    async fn list_snapshots_page(
        &self,
        team_name: &str,
        user_id: &str,
        limit: u32,
        cursor: Option<TeamSnapshotListCursor>,
    ) -> Result<TeamSnapshotListPage, String> {
        let limit = validate_team_snapshot_list_limit(limit);
        let mut qb = QueryBuilder::<MySql>::new(
            "SELECT snapshot_id, team_name, user_id, label, git_commit, \
                    session_id, team_definition_json, \
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%f') AS created_at \
             FROM team_snapshots \
             WHERE user_id = ",
        );
        qb.push_bind(user_id);
        qb.push(" AND team_name = ");
        qb.push_bind(team_name);
        if let Some(cursor) = &cursor {
            let created_at = team_snapshot_cursor_db_created_at(cursor)?;
            let snapshot_id = team_snapshot_cursor_snapshot_id(cursor)?;
            qb.push(" AND (created_at < ");
            qb.push_bind(created_at.clone());
            qb.push(" OR (created_at = ");
            qb.push_bind(created_at);
            qb.push(" AND snapshot_id < ");
            qb.push_bind(snapshot_id);
            qb.push("))");
        }
        qb.push(" ORDER BY created_at DESC, snapshot_id DESC LIMIT ");
        qb.push_bind(team_list_query_limit(limit));

        let rows = qb
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(|e| format!("snapshot page SELECT failed: {e}"))?;
        let mut snapshots = rows
            .iter()
            .map(row_to_team_snapshot_record)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = snapshots.len() > limit as usize;
        if has_more {
            snapshots.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            snapshots
                .last()
                .map(team_snapshot_cursor_from_record)
                .transpose()?
        } else {
            None
        };
        Ok(TeamSnapshotListPage {
            snapshots,
            limit,
            next_cursor,
        })
    }

    async fn find_snapshot(
        &self,
        snapshot_id: &str,
        user_id: &str,
    ) -> Result<Option<TeamSnapshotRecord>, String> {
        let row = sqlx::query(
            "SELECT snapshot_id, team_name, user_id, label, git_commit, \
                    session_id, team_definition_json, \
                    CAST(created_at AS CHAR) AS created_at \
             FROM team_snapshots \
             WHERE snapshot_id = ? AND user_id = ?",
        )
        .bind(snapshot_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("snapshot SELECT failed: {e}"))?;

        row.as_ref().map(row_to_team_snapshot_record).transpose()
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

fn row_decode_error(
    table: &'static str,
    column: &'static str,
    error: impl std::fmt::Display,
) -> String {
    format!("{table} row decode column `{column}`: {error}")
}

fn row_string(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> Result<String, String> {
    use sqlx::Row;

    row.try_get::<String, _>(column)
        .map_err(|error| row_decode_error(table, column, error))
}

fn row_optional_string(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> Result<Option<String>, String> {
    use sqlx::Row;

    row.try_get::<Option<String>, _>(column)
        .map_err(|error| row_decode_error(table, column, error))
}

fn parse_row_json<T: DeserializeOwned>(
    table: &'static str,
    column: &'static str,
    raw: &str,
) -> Result<T, String> {
    serde_json::from_str(raw).map_err(|error| row_decode_error(table, column, error))
}

fn row_to_team_definition(row: &sqlx::mysql::MySqlRow) -> Result<TeamDefinition, String> {
    const TABLE: &str = "team_definitions";

    let team_id = row_string(row, TABLE, "team_id")?;
    let user_id = row_string(row, TABLE, "user_id")?;
    let name = row_string(row, TABLE, "name")?;
    let description = row_string(row, TABLE, "description")?;
    let members_str = row_string(row, TABLE, "members_json")?;
    let context_str = row_string(row, TABLE, "context_json")?;
    let created_at = row_string(row, TABLE, "created_at")?;
    let updated_at = row_string(row, TABLE, "updated_at")?;

    let members: Vec<TeamMemberDef> = parse_row_json(TABLE, "members_json", &members_str)?;
    let context: HashMap<String, String> = parse_row_json(TABLE, "context_json", &context_str)?;

    Ok(TeamDefinition {
        team_id,
        user_id,
        name,
        description,
        members,
        context,
        created_at,
        updated_at,
    })
}

fn row_to_team_snapshot_record(row: &sqlx::mysql::MySqlRow) -> Result<TeamSnapshotRecord, String> {
    const TABLE: &str = "team_snapshots";

    Ok(TeamSnapshotRecord {
        snapshot_id: row_string(row, TABLE, "snapshot_id")?,
        team_name: row_string(row, TABLE, "team_name")?,
        user_id: row_string(row, TABLE, "user_id")?,
        label: row_string(row, TABLE, "label")?,
        git_commit: row_optional_string(row, TABLE, "git_commit")?,
        session_id: row_optional_string(row, TABLE, "session_id")?,
        team_definition_json: row_optional_string(row, TABLE, "team_definition_json")?,
        created_at: row_string(row, TABLE, "created_at")?,
    })
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
            team_id: format!("bt-rev-{user_id}"),
            user_id: user_id.to_string(),
            name: "review".to_string(),
            description: "Independent code reviews with aggregated findings".to_string(),
            members: vec![
                TeamMemberDef {
                    role: "correctness_reviewer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "Review the task for correctness and provide evidence-backed findings. Do not modify code."
                            .to_string(),
                    ),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: false,
                    max_delegation_depth: 0,
                    ..Default::default()
                },
                TeamMemberDef {
                    role: "reviewer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You review code for bugs, security issues, and correctness. \
                         Provide actionable feedback."
                            .to_string(),
                    ),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: true,
                    max_delegation_depth: 1,
                    ..Default::default()
                },
            ],
            context: HashMap::new(),
            created_at: now.to_string(),
            updated_at: now.to_string(),
        },
        TeamDefinition {
            team_id: format!("bt-res-{user_id}"),
            user_id: user_id.to_string(),
            name: "research".to_string(),
            description: "Deep research: explorer gathers info, synthesizer produces report"
                .to_string(),
            members: vec![
                TeamMemberDef {
                    role: "explorer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You search the codebase, read docs, and gather information. \
                         Output structured findings."
                            .to_string(),
                    ),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: false,
                    max_delegation_depth: 0,
                    ..Default::default()
                },
                TeamMemberDef {
                    role: "synthesizer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You synthesize findings into a coherent analysis report.".to_string(),
                    ),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: true,
                    max_delegation_depth: 1,
                    ..Default::default()
                },
            ],
            context: HashMap::new(),
            created_at: now.to_string(),
            updated_at: now.to_string(),
        },
        TeamDefinition {
            team_id: format!("bt-dev-{user_id}"),
            user_id: user_id.to_string(),
            name: "dev".to_string(),
            description:
                "Full development cycle: planner decomposes, implementer codes, tester verifies"
                    .to_string(),
            members: vec![
                TeamMemberDef {
                    role: "planner".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You decompose the task into subtasks with acceptance criteria."
                            .to_string(),
                    ),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: true,
                    max_delegation_depth: 1,
                    ..Default::default()
                },
                TeamMemberDef {
                    role: "implementer".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You implement code changes following the plan.".to_string(),
                    ),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: false,
                    max_delegation_depth: 0,
                    ..Default::default()
                },
                TeamMemberDef {
                    role: "tester".to_string(),
                    agent_id: None,
                    system_prompt: Some(
                        "You write and run tests, verifying acceptance criteria.".to_string(),
                    ),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: false,
                    max_delegation_depth: 0,
                    ..Default::default()
                },
            ],
            context: HashMap::new(),
            created_at: now.to_string(),
            updated_at: now.to_string(),
        },
    ]
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn selection(offering_id: &str) -> astra_turn_types::ModelSelection {
        astra_turn_types::ModelSelection {
            offering_id: offering_id.to_string(),
        }
    }

    fn test_team() -> TeamDefinition {
        TeamDefinition {
            team_id: "test-team-1".to_string(),
            user_id: "user-1".to_string(),
            name: "test-team".to_string(),
            description: "A test team".to_string(),
            members: vec![
                TeamMemberDef {
                    role: "coder".to_string(),
                    agent_id: Some("coder-agent".to_string()),
                    system_prompt: None,
                    skills: vec!["edit".to_string()],
                    allow_tools: Some(vec!["read_file".to_string()]),
                    read_only: true,
                    initial_turns: Some(2),
                    max_turns: Some(5),
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: false,
                    max_delegation_depth: 0,
                },
                TeamMemberDef {
                    role: "reviewer".to_string(),
                    agent_id: None,
                    system_prompt: Some("Review carefully".to_string()),
                    skills: vec!["review-changes".to_string()],
                    model_selection: Some(selection("offer-claude-opus")),
                    mcp_servers: vec!["github".to_string()],
                    can_delegate: false,
                    max_delegation_depth: 0,
                    ..Default::default()
                },
            ],
            context: HashMap::from([("project".to_string(), "test-project".to_string())]),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn owner_initialization_cache_singleflights_concurrent_success() {
        let cache = Arc::new(OwnerInitializationCache::new(8));
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            let gate = Arc::clone(&gate);
            tasks.push(tokio::spawn(async move {
                gate.wait().await;
                cache
                    .ensure("alice", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Ok(())
                    })
                    .await
            }));
        }

        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn owner_initialization_cache_retries_failures_without_growing_unbounded() {
        let cache = OwnerInitializationCache::new(2);
        let calls = AtomicUsize::new(0);

        let first = cache
            .ensure("alice", || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("transient database failure".to_string())
            })
            .await;
        assert_eq!(first, Err("transient database failure".to_string()));
        cache
            .ensure("alice", || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        cache.ensure("bob", || async { Ok(()) }).await.unwrap();
        cache.ensure("carol", || async { Ok(()) }).await.unwrap();
        assert_eq!(cache.len().await, 2);

        let alice_reinitialized = AtomicUsize::new(0);
        cache
            .ensure("alice", || async {
                alice_reinitialized.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(alice_reinitialized.load(Ordering::SeqCst), 1);
        assert_eq!(cache.len().await, 2);
    }

    // ── Resolve ──

    #[test]
    fn resolve_member_with_explicit_agent_id() {
        let team = test_team();
        let profile = resolve_member_to_profile(&team.members[0], &team);
        assert_eq!(profile.agent_id, "coder-agent");
        assert_eq!(profile.skill_filter, vec!["edit"]);
        assert_eq!(profile.allow_tools, Some(vec!["read_file".to_string()]));
        assert!(profile.read_only);
        assert_eq!(profile.initial_turns, Some(2));
        assert_eq!(profile.max_turns, Some(5));
        assert!(profile.model_selection.is_none());
        assert_eq!(profile.tier, AgentTier::User);
        assert!(!profile.can_delegate);
    }

    #[test]
    fn resolve_member_auto_generates_agent_id() {
        let team = test_team();
        let profile = resolve_member_to_profile(&team.members[1], &team);
        assert_eq!(profile.agent_id, "team-test-team-reviewer");
        assert_eq!(
            profile
                .model_selection
                .as_ref()
                .map(|selection| selection.offering_id.as_str()),
            Some("offer-claude-opus")
        );
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

    #[test]
    fn validate_team_rejects_non_positive_member_turn_controls() {
        let mut team = test_team();
        team.members[0].initial_turns = Some(0);
        let errors = validate_team(&team).unwrap_err();
        assert!(errors.iter().any(|error| {
            matches!(error, TeamValidationError::InvalidMember(message)
                if message.contains("initial_turns must be positive"))
        }));
    }

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
    async fn ensure_builtins_is_owner_scoped_idempotent_and_non_destructive() {
        let store = InMemoryTeamStore::new();
        let mut existing_review = test_team();
        existing_review.team_id = "alice-custom-review".to_string();
        existing_review.user_id = "alice".to_string();
        existing_review.name = "review".to_string();
        existing_review.description = "owner customized review".to_string();
        store.save_team(&existing_review).await.unwrap();

        store.ensure_builtins("alice").await.unwrap();
        store.ensure_builtins("alice").await.unwrap();
        store.ensure_builtins("bob").await.unwrap();

        let alice = store.list_teams("alice").await.unwrap();
        let bob = store.list_teams("bob").await.unwrap();
        assert_eq!(alice.len(), 3);
        assert_eq!(bob.len(), 3);
        assert_eq!(
            alice
                .iter()
                .find(|team| team.name == "review")
                .map(|team| (team.team_id.as_str(), team.description.as_str())),
            Some(("alice-custom-review", "owner customized review")),
            "lazy initialization must not overwrite an owner's existing definition"
        );
        assert!(
            alice
                .iter()
                .all(|team| team.user_id == "alice" && !team.team_id.contains("bob"))
        );
        assert!(bob.iter().all(|team| team.user_id == "bob"));
    }

    #[tokio::test]
    async fn in_memory_store_user_isolation() {
        let store = InMemoryTeamStore::with_builtins("u1");
        let teams = store.list_teams("u2").await.unwrap();
        assert!(teams.is_empty());
    }

    #[test]
    fn matrixone_team_list_sql_is_bounded() {
        assert_eq!(MAX_TEAM_LIST_ROWS, 200);
        assert!(
            TEAM_LIST_SELECT_SQL
                .to_ascii_uppercase()
                .contains("LIMIT ?"),
            "team list must not read all team_definitions rows for a user"
        );
        assert!(
            TEAM_LIST_SELECT_SQL.contains("ORDER BY name"),
            "bounded team list must keep deterministic ordering"
        );
    }

    #[tokio::test]
    async fn in_memory_store_list_teams_is_bounded_and_sorted() {
        let store = InMemoryTeamStore::new();
        for idx in (0..(MAX_TEAM_LIST_ROWS + 5)).rev() {
            let mut team = test_team();
            team.team_id = format!("team-{idx:03}");
            team.name = format!("team-{idx:03}");
            store.save_team(&team).await.unwrap();
        }

        let teams = store.list_teams("user-1").await.unwrap();
        assert_eq!(teams.len(), MAX_TEAM_LIST_ROWS);
        assert_eq!(
            teams.first().map(|team| team.name.as_str()),
            Some("team-000")
        );
        assert_eq!(
            teams.last().map(|team| team.name.as_str()),
            Some("team-199")
        );
        assert!(
            teams.iter().all(|team| team.name.as_str() < "team-200"),
            "list should return the first bounded page in name order"
        );
    }

    // ── Builtins ──

    #[test]
    fn builtin_member_profiles_do_not_require_workspace_skills() {
        for team in builtin_teams("u1", "2026-01-01T00:00:00Z") {
            for member in &team.members {
                let profile = resolve_member_to_profile(member, &team);
                assert!(
                    profile.skill_filter.is_empty(),
                    "builtin {}/{} requires skills absent from an ordinary workspace",
                    team.name,
                    member.role
                );
            }
        }
        // Explicit user requirements remain part of the admitted profile.
        let mut team = test_team();
        team.members[0].skills = vec!["user-required-skill".into()];
        assert_eq!(
            resolve_member_to_profile(&team.members[0], &team).skill_filter,
            vec!["user-required-skill"]
        );
    }

    #[test]
    fn members_json_roundtrip() {
        let members = vec![
            TeamMemberDef {
                role: "coder".to_string(),
                agent_id: Some("my-coder".to_string()),
                system_prompt: None,
                skills: vec!["edit".to_string(), "test".to_string()],
                model_selection: Some(selection("offer-gpt-4")),
                mcp_servers: vec!["github".to_string()],
                can_delegate: false,
                max_delegation_depth: 0,
                ..Default::default()
            },
            TeamMemberDef {
                role: "reviewer".to_string(),
                agent_id: None,
                system_prompt: Some("Be thorough".to_string()),
                skills: vec![],
                model_selection: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
                ..Default::default()
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
    fn context_json_handles_empty() {
        let empty: HashMap<String, String> = HashMap::new();
        let json = serde_json::to_string(&empty).unwrap();
        assert_eq!(json, "{}");
        let parsed: HashMap<String, String> = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn team_snapshot_cursor_accepts_db_and_rfc3339_timestamps() {
        let db_cursor = TeamSnapshotListCursor {
            created_at: "2026-10-01T12:34:56.123456".to_string(),
            snapshot_id: "snap-1".to_string(),
        };
        assert_eq!(
            team_snapshot_cursor_db_created_at(&db_cursor).unwrap(),
            "2026-10-01 12:34:56.123456"
        );
        assert_eq!(
            team_snapshot_cursor_snapshot_id(&db_cursor).unwrap(),
            "snap-1"
        );

        let rfc3339_cursor = TeamSnapshotListCursor {
            created_at: "2026-10-01T20:34:56.123456+08:00".to_string(),
            snapshot_id: "snap-1".to_string(),
        };
        assert_eq!(
            team_snapshot_cursor_db_created_at(&rfc3339_cursor).unwrap(),
            "2026-10-01 12:34:56.123456"
        );

        let missing_id = TeamSnapshotListCursor {
            created_at: "2026-10-01T12:34:56.123456".to_string(),
            snapshot_id: " ".to_string(),
        };
        assert!(team_snapshot_cursor_snapshot_id(&missing_id).is_err());
    }

    fn test_snapshot_record(snapshot_id: &str, created_at: &str) -> TeamSnapshotRecord {
        TeamSnapshotRecord {
            snapshot_id: snapshot_id.to_string(),
            team_name: "team-a".to_string(),
            user_id: "user-1".to_string(),
            label: format!("snapshot {snapshot_id}"),
            git_commit: None,
            session_id: None,
            team_definition_json: None,
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn team_snapshot_page_uses_stable_seek_cursor() {
        let snapshots = vec![
            test_snapshot_record("snap-a", "2026-10-01T12:00:00.000000"),
            test_snapshot_record("snap-c", "2026-10-01T12:00:00.000000"),
            test_snapshot_record("snap-b", "2026-10-01T12:00:00.000000"),
            test_snapshot_record("snap-old", "2026-09-30T12:00:00.000000"),
        ];

        let first = team_snapshot_page_from_records(snapshots.clone(), 2, None).unwrap();
        assert_eq!(
            first
                .snapshots
                .iter()
                .map(|snapshot| snapshot.snapshot_id.as_str())
                .collect::<Vec<_>>(),
            vec!["snap-c", "snap-b"]
        );
        assert_eq!(
            first.next_cursor,
            Some(TeamSnapshotListCursor {
                created_at: "2026-10-01T12:00:00.000000".to_string(),
                snapshot_id: "snap-b".to_string(),
            })
        );

        let second = team_snapshot_page_from_records(snapshots, 2, first.next_cursor).unwrap();
        assert_eq!(
            second
                .snapshots
                .iter()
                .map(|snapshot| snapshot.snapshot_id.as_str())
                .collect::<Vec<_>>(),
            vec!["snap-a", "snap-old"]
        );
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn full_team_definition_serde_roundtrip() {
        let team = test_team();
        let json = serde_json::to_string(&team).unwrap();
        let parsed: TeamDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.team_id, team.team_id);
        assert_eq!(parsed.name, team.name);
        assert_eq!(parsed.members.len(), 2);
        assert!(parsed.context.contains_key("project"));
    }

    // ─── T-2: Validation Tests ─────────────────────────────────────────────

    #[test]
    fn draft_team_is_valid() {
        let mut team = test_team();
        team.members.clear();
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
    fn resolve_member_generates_auto_id() {
        let team = test_team();
        let member = &team.members[1]; // reviewer, agent_id=None
        let profile = resolve_member_to_profile(member, &team);
        assert_eq!(profile.agent_id, "team-test-team-reviewer");
    }

    #[test]
    fn resolve_member_creates_fresh_profile() {
        let team = test_team();
        let member = &team.members[1]; // reviewer, agent_id=None
        let profile = resolve_member_to_profile(member, &team);

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

        let profile = resolve_member_to_profile(&member, &team);
        assert_eq!(profile.tier, AgentTier::System);
        assert!(profile.can_delegate);
        assert_eq!(profile.max_delegation_depth, 2);
    }

    #[test]
    fn can_delegate_false_keeps_user_tier() {
        let team = test_team();
        let member = &team.members[0]; // can_delegate = false
        let profile = resolve_member_to_profile(member, &team);
        assert_eq!(profile.tier, AgentTier::User);
        assert!(!profile.can_delegate);
    }

    // ─── T-2: Metadata injection ───────────────────────────────────────────

    #[test]
    fn metadata_includes_team_name_and_role() {
        let team = test_team();
        let member = &team.members[0]; // coder
        let profile = resolve_member_to_profile(member, &team);

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
        let profile = resolve_member_to_profile(member, &team);

        let ctx = profile.metadata.get("team_context").unwrap();
        let ctx_map: HashMap<String, String> = serde_json::from_value(ctx.clone()).unwrap();
        assert_eq!(ctx_map.get("project"), Some(&"test-project".to_string()));
    }

    #[test]
    fn metadata_empty_context_no_team_context_key() {
        let mut team = test_team();
        team.context.clear();
        let member = &team.members[0];
        let profile = resolve_member_to_profile(member, &team);

        // team_name and team_role always present
        assert!(profile.metadata.contains_key("team_name"));
        assert!(profile.metadata.contains_key("team_role"));
        // team_context absent when context is empty
        assert!(!profile.metadata.contains_key("team_context"));
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
            "model_selection": null,
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
            "model_selection": null,
            "mcp_servers": [],
            "can_delegate": true,
            "max_delegation_depth": 3
        }"#;
        let member: TeamMemberDef = serde_json::from_str(json).unwrap();
        assert!(member.can_delegate);
        assert_eq!(member.max_delegation_depth, 3);
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
