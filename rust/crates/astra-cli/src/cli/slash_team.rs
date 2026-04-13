use super::*;
use crate::{current_access_token, is_session_not_found_error};
use astra_runtime::server::team_orchestrator::ExecutionPhase;
#[allow(unused_imports)]
use astra_services::team_persistence::{TeamPersistenceService, WorktreeMode};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

// ── Team History & Snapshot Tracking ────────────────────────────────────

/// Record of a past team execution.
#[derive(Clone, Debug)]
pub(super) struct TeamHistoryEntry {
    pub team_name: String,
    pub task: String,
    pub delegation_id: String,
    pub parent_run_id: String,
    pub status: String,
    pub agent_count: usize,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub error: Option<String>,
    pub started_at: String,
}

/// A saved snapshot associated with a team.
#[derive(Clone, Debug)]
pub(super) struct TeamSnapshotEntry {
    pub snapshot_id: String,
    pub team_name: String,
    pub label: String,
    pub git_commit: Option<String>,
    pub session_id: Option<String>,
    pub created_at: String,
}

// ── Team Registry ───────────────────────────────────────────────────────

/// A named team of agent roles that can coordinate on tasks.
#[derive(Clone, Debug)]
pub(super) struct Team {
    /// Stable identifier persisted across runs (UUID assigned at creation).
    pub team_id: String,
    pub name: String,
    pub description: String,
    pub members: Vec<TeamMember>,
    pub shared_context: HashMap<String, String>,
    pub worktree_mode: WorktreeMode,
    /// Explicit coordination mode (None = auto-infer from roles).
    pub coordination: Option<astra_services::team_persistence::TeamCoordination>,
    pub created_at: String,
}

/// A member role within a team.
#[derive(Clone, Debug)]
pub(super) struct TeamMember {
    pub role: String,
    pub description: String,
    pub skills: Vec<String>,
    pub model_override: Option<String>,
}

/// Registry of all defined teams (stored in ReplState).
#[derive(Clone, Debug)]
pub(super) struct TeamRegistry {
    teams: HashMap<String, Team>,
    pub history: Vec<TeamHistoryEntry>,
    snapshots: Vec<TeamSnapshotEntry>,
    /// Whether we've loaded teams from the persistence store yet.
    pub store_loaded: bool,
}

impl Default for TeamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TeamRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            teams: HashMap::new(),
            history: Vec::new(),
            snapshots: Vec::new(),
            store_loaded: false,
        };
        // Register built-in team templates
        reg.register_builtins();
        reg
    }

    fn register_builtins(&mut self) {
        // Code review team: producer + reviewer
        self.teams.insert(
            "review".to_string(),
            Team {
                team_id: uuid::Uuid::new_v4().to_string(),
                name: "review".to_string(),
                description: "Adversarial code review: one agent writes, another reviews".into(),
                members: vec![
                    TeamMember {
                        role: "producer".to_string(),
                        description: "Writes or modifies code to fulfill the task".into(),
                        skills: vec!["review-changes".into()],
                        model_override: None,
                    },
                    TeamMember {
                        role: "reviewer".to_string(),
                        description: "Reviews code for bugs, security, and correctness".into(),
                        skills: vec!["review-changes".into()],
                        model_override: None,
                    },
                ],
                shared_context: HashMap::new(),
                worktree_mode: WorktreeMode::Shared,
                coordination: None,
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        );

        // Research team: explorer + synthesizer
        self.teams.insert(
            "research".to_string(),
            Team {
                team_id: uuid::Uuid::new_v4().to_string(),
                name: "research".to_string(),
                description: "Deep research: explorer gathers info, synthesizer produces report"
                    .into(),
                members: vec![
                    TeamMember {
                        role: "explorer".to_string(),
                        description: "Searches codebase, reads docs, gathers information".into(),
                        skills: vec!["analyze-session".into()],
                        model_override: None,
                    },
                    TeamMember {
                        role: "synthesizer".to_string(),
                        description: "Synthesizes findings into coherent analysis".into(),
                        skills: vec![],
                        model_override: None,
                    },
                ],
                shared_context: HashMap::new(),
                worktree_mode: WorktreeMode::Shared,
                coordination: None,
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        );

        // Full dev team: planner + implementer + tester
        self.teams.insert(
            "dev".to_string(),
            Team {
                team_id: uuid::Uuid::new_v4().to_string(),
                name: "dev".to_string(),
                description:
                    "Full development cycle: planner decomposes, implementer codes, tester verifies"
                        .into(),
                members: vec![
                    TeamMember {
                        role: "planner".to_string(),
                        description: "Decomposes task into subtasks with acceptance criteria"
                            .into(),
                        skills: vec![],
                        model_override: None,
                    },
                    TeamMember {
                        role: "implementer".to_string(),
                        description: "Implements code changes following the plan".into(),
                        skills: vec![],
                        model_override: None,
                    },
                    TeamMember {
                        role: "tester".to_string(),
                        description: "Writes and runs tests, verifies acceptance criteria".into(),
                        skills: vec!["verify-task".into()],
                        model_override: None,
                    },
                ],
                shared_context: HashMap::new(),
                worktree_mode: WorktreeMode::Isolated,
                coordination: None,
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    /// Merge teams from persistence store into the registry (idempotent).
    pub fn merge_from_store(
        &mut self,
        teams: Vec<astra_services::team_persistence::TeamDefinition>,
    ) {
        for def in teams {
            if self.teams.contains_key(&def.name) {
                continue; // don't overwrite in-memory edits
            }
            self.teams.insert(
                def.name.clone(),
                Team {
                    team_id: def.team_id,
                    name: def.name,
                    description: def.description,
                    members: def
                        .members
                        .iter()
                        .map(|m| TeamMember {
                            role: m.role.clone(),
                            description: m
                                .system_prompt
                                .clone()
                                .unwrap_or_else(|| format!("{} agent", m.role)),
                            skills: m.skills.clone(),
                            model_override: m.model_override.clone(),
                        })
                        .collect(),
                    shared_context: def.context,
                    worktree_mode: def.worktree_mode,
                    coordination: Some(def.coordination),
                    created_at: def.created_at,
                },
            );
        }
    }

    pub fn get(&self, name: &str) -> Option<&Team> {
        self.teams.get(name)
    }

    pub fn list(&self) -> Vec<&Team> {
        let mut teams: Vec<_> = self.teams.values().collect();
        teams.sort_by_key(|t| &t.name);
        teams
    }

    pub fn create(
        &mut self,
        name: String,
        description: String,
        coordination: Option<astra_services::team_persistence::TeamCoordination>,
    ) -> Result<(), String> {
        if self.teams.contains_key(&name) {
            return Err(format!("Team '{name}' already exists"));
        }
        self.teams.insert(
            name.clone(),
            Team {
                team_id: uuid::Uuid::new_v4().to_string(),
                name,
                description,
                members: Vec::new(),
                shared_context: HashMap::new(),
                worktree_mode: WorktreeMode::Shared,
                coordination,
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        Ok(())
    }

    pub fn add_member(&mut self, team: &str, member: TeamMember) -> Result<(), String> {
        let t = self
            .teams
            .get_mut(team)
            .ok_or_else(|| format!("Team '{team}' not found"))?;
        if t.members.iter().any(|m| m.role == member.role) {
            return Err(format!(
                "Role '{}' already exists in team '{team}'",
                member.role
            ));
        }
        t.members.push(member);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        if self.teams.remove(name).is_none() {
            return Err(format!("Team '{name}' not found"));
        }
        Ok(())
    }

    pub fn set_context(&mut self, team: &str, key: String, value: String) -> Result<(), String> {
        let t = self
            .teams
            .get_mut(team)
            .ok_or_else(|| format!("Team '{team}' not found"))?;
        t.shared_context.insert(key, value);
        Ok(())
    }

    pub fn record_execution(&mut self, entry: TeamHistoryEntry) {
        self.history.push(entry);
    }

    pub fn get_history(&self, team_name: &str) -> Vec<&TeamHistoryEntry> {
        self.history
            .iter()
            .filter(|e| e.team_name == team_name)
            .collect()
    }

    pub fn add_snapshot(&mut self, entry: TeamSnapshotEntry) {
        self.snapshots.push(entry);
    }

    pub fn get_snapshots(&self, team_name: &str) -> Vec<&TeamSnapshotEntry> {
        self.snapshots
            .iter()
            .filter(|s| s.team_name == team_name)
            .collect()
    }

    pub fn find_snapshot(&self, snapshot_id: &str) -> Option<&TeamSnapshotEntry> {
        self.snapshots.iter().find(|s| s.snapshot_id == snapshot_id)
    }
}

// ── CLI Team → TeamDefinition Conversion ────────────────────────────────

/// Get current git HEAD commit SHA (best-effort).
fn git_head_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Convert a CLI [`Team`] to a runtime [`TeamDefinition`] for the orchestrator.
///
/// Maps the CLI team's built-in coordination heuristic (based on member count
/// and role names) to the appropriate [`TeamCoordination`] variant.
fn cli_team_to_definition(
    team: &Team,
    user_id: &str,
) -> astra_services::team_persistence::TeamDefinition {
    use astra_services::team_persistence::*;

    let coordination = team
        .coordination
        .clone()
        .unwrap_or_else(|| infer_coordination(team));
    let now = chrono::Utc::now().to_rfc3339();
    TeamDefinition {
        team_id: team.team_id.clone(),
        user_id: user_id.to_string(),
        name: team.name.clone(),
        description: team.description.clone(),
        coordination,
        members: team
            .members
            .iter()
            .map(|m| TeamMemberDef {
                role: m.role.clone(),
                agent_id: None,
                system_prompt: Some(m.description.clone()),
                skills: m.skills.clone(),
                model_override: m.model_override.clone(),
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
            })
            .collect(),
        context: team.shared_context.clone(),
        worktree_mode: team.worktree_mode.clone(),
        budget: None,
        max_parallel: 0,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// Infer coordination pattern from team structure and member roles.
fn infer_coordination(team: &Team) -> astra_services::team_persistence::TeamCoordination {
    use astra_services::team_persistence::TeamCoordination;

    let roles: Vec<&str> = team.members.iter().map(|m| m.role.as_str()).collect();

    // Adversarial: if roles contain producer+reviewer
    if roles
        .iter()
        .any(|r| r.contains("producer") || r.contains("writer"))
        && roles
            .iter()
            .any(|r| r.contains("reviewer") || r.contains("critic"))
    {
        return TeamCoordination::Adversarial {
            max_rounds: 3,
            threshold: 0.8,
        };
    }

    // Pipeline: if members appear to be sequential stages
    if team.members.len() >= 2
        && roles.iter().any(|r| {
            r.contains("analyst")
                || r.contains("planner")
                || r.contains("explorer")
                || r.contains("researcher")
        })
        && roles.iter().any(|r| {
            r.contains("synthesizer") || r.contains("implementer") || r.contains("executor")
        })
    {
        return TeamCoordination::Pipeline;
    }

    // Default: FanOut for parallel teams
    TeamCoordination::FanOut {
        aggregation: "merge".to_string(),
    }
}

// ── Slash Command Handler ───────────────────────────────────────────────

async fn ensure_team_run_session(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut super::ReplState,
) -> Result<String, String> {
    let token = current_access_token(profile).ok_or_else(|| "Not logged in".to_string())?;
    if let Some(session_id) = state.session_id.clone() {
        match api.get_session_text(&token, &session_id).await {
            Ok(_) => return Ok(session_id),
            Err(err) => {
                let err = map_thin_err(err);
                if !is_session_not_found_error(&err) {
                    return Ok(session_id);
                }
                let _ = crate::auth_flow::clear_profile_last_session(profile);
                state.session_id = None;
                state.unregister_root_mailbox().await;
                state.run_id = None;
                state.journal = None;
            }
        }
    }

    let body = api
        .post_sessions_json(&token, &serde_json::json!({}))
        .await
        .map_err(map_thin_err)?;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let session_id = value
        .get("session_id")
        .or_else(|| value.get("id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "session create response missing session_id".to_string())?
        .to_string();

    crate::repl_turn::initialize_journal_pub(state, &session_id);
    crate::repl_turn::persist_last_session_id(profile, &session_id);
    state.session_id = Some(session_id.clone());
    Ok(session_id)
}

pub(super) async fn handle_team_command(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut super::ReplState,
) {
    // Hydrate registry from persistence store on first command
    if !state.team_registry.store_loaded {
        let user_id = state
            .ingestion_user_id
            .clone()
            .unwrap_or_else(|| "local".into());
        if let Ok(teams) = state.team_store.list_teams(&user_id).await {
            state.team_registry.merge_from_store(teams);
        }
        state.team_registry.store_loaded = true;
    }

    let mut parts = arg.splitn(2, ' ');
    let sub = parts.next().unwrap_or("").trim();
    let sub_arg = parts.next().unwrap_or("").trim();

    match sub {
        "" | "help" => {
            eprintln!(
                "\n{}",
                "─── Team ───────────────────────────────────────"
                    .bold()
                    .cyan()
            );
            let teams = state.team_registry.list();
            let names = teams
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "  {:<16} {}",
                "teams:".dim(),
                if names.is_empty() {
                    "(none)".dim().to_string()
                } else {
                    names.cyan().to_string()
                }
            );
            eprintln!(
                "  {:<16} {}",
                "built-ins:".dim(),
                "review, research, dev".cyan()
            );
            eprintln!();
            eprintln!("  {}", team_subcommands_hint().dim());
            eprintln!("  {}", "Examples:".dim());
            eprintln!("    {}", "/team info review".cyan());
            eprintln!("    {}", "/team run review review the latest diff".cyan());
            eprintln!("    {}", "/team snapshot dev before-refactor".cyan());
            eprintln!();
        }

        "list" => {
            let teams = state.team_registry.list();
            if teams.is_empty() {
                eprintln!(
                    "  {}",
                    "No teams defined. Use /team create <name> <description>".dim()
                );
                return;
            }
            eprintln!(
                "\n{}",
                "─── Teams ───────────────────────────────────────────────"
                    .bold()
                    .cyan()
            );
            for t in &teams {
                let member_names: Vec<_> = t.members.iter().map(|m| m.role.as_str()).collect();
                eprintln!(
                    "\n  {} {}",
                    t.name.as_str().cyan().bold(),
                    format!("({})", t.description).dim()
                );
                if member_names.is_empty() {
                    eprintln!("    {}", "No members. Use /team add-member".dim());
                } else {
                    for m in &t.members {
                        eprintln!(
                            "    {} {} {}",
                            "•".dim(),
                            m.role.as_str().green(),
                            format!("— {}", m.description).dim()
                        );
                    }
                }
                if !t.shared_context.is_empty() {
                    eprintln!(
                        "    {} shared keys: {}",
                        "📎".to_string().dim(),
                        t.shared_context
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                            .dim()
                    );
                }
            }
            eprintln!();
        }

        "create" => {
            // /team create <name> [--mode pipeline|adversarial|fanout|sequential] [description]
            let mut parts = sub_arg.splitn(2, ' ');
            let name = parts.next().unwrap_or("").trim();
            let rest = parts.next().unwrap_or("").trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /team create <name> [--mode pipeline|adversarial|fanout|sequential] [description]".yellow());
                return;
            }
            let (coordination, desc) = if rest.starts_with("--mode ") {
                let after_flag = &rest[7..];
                let mut mode_parts = after_flag.splitn(2, ' ');
                let mode_str = mode_parts.next().unwrap_or("");
                let d = mode_parts.next().unwrap_or("").trim();
                let coord = match mode_str {
                    "pipeline" => {
                        Some(astra_services::team_persistence::TeamCoordination::Pipeline)
                    }
                    "adversarial" => Some(
                        astra_services::team_persistence::TeamCoordination::Adversarial {
                            max_rounds: 3,
                            threshold: 0.8,
                        },
                    ),
                    "fanout" | "fan-out" => {
                        Some(astra_services::team_persistence::TeamCoordination::FanOut {
                            aggregation: "merge".to_string(),
                        })
                    }
                    "sequential" => Some(
                        astra_services::team_persistence::TeamCoordination::Sequential {
                            stop_on_success: false,
                        },
                    ),
                    other => {
                        eprintln!(
                            "  {} Unknown mode '{}'. Options: pipeline, adversarial, fanout, sequential",
                            theme::icon_err(),
                            other
                        );
                        return;
                    }
                };
                (coord, d)
            } else {
                (None, rest)
            };
            let description = if desc.is_empty() {
                format!("Custom team: {name}")
            } else {
                desc.to_string()
            };
            match state
                .team_registry
                .create(name.to_string(), description, coordination)
            {
                Ok(()) => {
                    // Persist so team survives restart
                    let user_id = state
                        .ingestion_user_id
                        .clone()
                        .unwrap_or_else(|| "local".into());
                    if let Some(t) = state.team_registry.get(name) {
                        let def = cli_team_to_definition(t, &user_id);
                        if let Err(e) = state.team_store.save_team(&def).await {
                            eprintln!("  {} persist warning: {e}", theme::icon_warn());
                        }
                    }
                    eprintln!(
                        "  {} Team '{}' created. Add members with /team add-member {} <role> <description>",
                        theme::icon_ok(),
                        name.cyan(),
                        name
                    );
                }
                Err(e) => eprintln!("  {} {e}", theme::icon_err()),
            }
        }

        "add-member" => {
            // /team add-member <team> <role> <description>
            let mut parts = sub_arg.splitn(3, ' ');
            let team = parts.next().unwrap_or("").trim();
            let role = parts.next().unwrap_or("").trim();
            let desc = parts.next().unwrap_or("").trim();
            if team.is_empty() || role.is_empty() {
                eprintln!(
                    "{}",
                    "  Usage: /team add-member <team> <role> [description]".yellow()
                );
                return;
            }
            let member = TeamMember {
                role: role.to_string(),
                description: if desc.is_empty() {
                    format!("{role} agent")
                } else {
                    desc.to_string()
                },
                skills: Vec::new(),
                model_override: None,
            };
            match state.team_registry.add_member(team, member) {
                Ok(()) => {
                    // Sync to persistence store
                    let user_id = state
                        .ingestion_user_id
                        .clone()
                        .unwrap_or_else(|| "local".into());
                    if let Some(t) = state.team_registry.get(team) {
                        let def = cli_team_to_definition(t, &user_id);
                        if let Err(e) = state.team_store.save_team(&def).await {
                            eprintln!("  {} persist warning: {e}", theme::icon_warn());
                        }
                    }
                    eprintln!(
                        "  {} Added role '{}' to team '{}'",
                        theme::icon_ok(),
                        role.green(),
                        team.cyan()
                    );
                }
                Err(e) => eprintln!("  {} {e}", theme::icon_err()),
            }
        }

        "info" => {
            let name = sub_arg.trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /team info <name>".yellow());
                return;
            }
            match state.team_registry.get(name) {
                Some(t) => {
                    eprintln!("\n  {} {}", "Team:".bold(), t.name.as_str().cyan().bold());
                    eprintln!("  {} {}", "Description:".dim(), t.description);
                    eprintln!("  {} {}", "Created:".dim(), t.created_at);
                    if let Some(ref coord) = t.coordination {
                        eprintln!("  {} {:?}", "Coordination:".dim(), coord);
                    }
                    eprintln!("\n  {}", "Members:".bold());
                    for m in &t.members {
                        eprintln!(
                            "    {} {} — {}",
                            "•".dim(),
                            m.role.as_str().green(),
                            m.description
                        );
                        if !m.skills.is_empty() {
                            eprintln!("      {} {}", "Skills:".dim(), m.skills.join(", "));
                        }
                        if let Some(ref model) = m.model_override {
                            eprintln!("      {} {}", "Model:".dim(), model);
                        }
                    }
                    if !t.shared_context.is_empty() {
                        eprintln!("\n  {}", "Shared Context:".bold());
                        for (k, v) in &t.shared_context {
                            let preview = truncate_str(v, 60);
                            eprintln!("    {} = {}", k.as_str().cyan(), preview);
                        }
                    }
                    eprintln!();
                }
                None => {
                    eprintln!("  {} Team '{}' not found", theme::icon_err(), name);
                }
            }
        }

        "delete" => {
            let name = sub_arg.trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /team delete <name>".yellow());
                return;
            }
            match state.team_registry.remove(name) {
                Ok(()) => {
                    // Also delete from persistence store (best-effort)
                    let user_id = state
                        .ingestion_user_id
                        .clone()
                        .unwrap_or_else(|| "local".into());
                    let _ = state.team_store.delete_team(&user_id, name).await;
                    eprintln!("  {} Team '{}' deleted", theme::icon_ok(), name);
                }
                Err(e) => eprintln!("  {} {e}", theme::icon_err()),
            }
        }

        "context" => {
            // /team context <team> <key> <value>
            let mut parts = sub_arg.splitn(3, ' ');
            let team = parts.next().unwrap_or("").trim();
            let key = parts.next().unwrap_or("").trim();
            let value = parts.next().unwrap_or("").trim();
            if team.is_empty() || key.is_empty() {
                eprintln!("{}", "  Usage: /team context <team> <key> <value>".yellow());
                eprintln!(
                    "{}",
                    "  Sets shared context accessible to all team members.".dim()
                );
                return;
            }
            match state
                .team_registry
                .set_context(team, key.to_string(), value.to_string())
            {
                Ok(()) => {
                    eprintln!(
                        "  {} Set context '{}'='{}' on team '{}'",
                        theme::icon_ok(),
                        key,
                        truncate_str(value, 40),
                        team.cyan()
                    );
                }
                Err(e) => eprintln!("  {} {e}", theme::icon_err()),
            }
        }

        "run" => {
            // /team run <team> <task description> [--mock <scenario>]
            let mut parts = sub_arg.splitn(2, ' ');
            let team_name = parts.next().unwrap_or("").trim();
            let rest = parts.next().unwrap_or("").trim();

            // Parse optional --mock flag (must be a standalone word, not inside task text)
            let (task, mock_scenario) = {
                let words: Vec<&str> = rest.split_whitespace().collect();
                if let Some(pos) = words.iter().position(|&w| w == "--mock") {
                    let task_part = words[..pos].join(" ");
                    let scenario_name = words.get(pos + 1).copied().unwrap_or("complete");
                    let scenario = super::mock_llm::MockScenario::from_str(scenario_name)
                        .unwrap_or_else(|| {
                            eprintln!(
                                "  {} Unknown mock scenario '{}'. Available: {}",
                                theme::icon_warn(),
                                scenario_name,
                                super::mock_llm::MockScenario::all()
                                    .iter()
                                    .map(|(n, _)| *n)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                            super::mock_llm::MockScenario::Complete
                        });
                    (task_part, Some(scenario))
                } else {
                    (rest.to_string(), None)
                }
            };
            let task = task.trim();

            if team_name.is_empty() || task.is_empty() {
                eprintln!(
                    "{}",
                    "  Usage: /team run <team> <task description> [--mock <scenario>]".yellow()
                );
                eprintln!(
                    "{}",
                    "  Executes a team task through the delegation engine.".dim()
                );
                eprintln!("  Mock scenarios (bypass LLM, test orchestration only):");
                for (name, desc) in super::mock_llm::MockScenario::all() {
                    eprintln!("    --mock {:<20} {}", name, desc);
                }
                return;
            }

            let cli_team = match state.team_registry.get(team_name) {
                Some(t) => t.clone(),
                None => {
                    eprintln!("  {} Team '{}' not found", theme::icon_err(), team_name);
                    return;
                }
            };

            if cli_team.members.is_empty() {
                eprintln!(
                    "  {} Team '{}' has no members. Use /team add-member to add agents first.",
                    theme::icon_err(),
                    team_name
                );
                return;
            }

            let user_id = state
                .ingestion_user_id
                .clone()
                .unwrap_or_else(|| "local".into());
            let session_id = match ensure_team_run_session(api, profile, state).await {
                Ok(session_id) => session_id,
                Err(e) => {
                    eprintln!("  {} Failed to prepare session: {e}", theme::icon_err());
                    return;
                }
            };

            // Convert CLI team → TeamDefinition for the orchestrator
            let team_def = cli_team_to_definition(&cli_team, &user_id);

            // Use the shared team store; ensure this team is loaded into it
            let team_store = state.team_store.clone();
            if let Err(e) = team_store.save_team(&team_def).await {
                eprintln!("  {} Failed to prepare team store: {e}", theme::icon_err());
                return;
            }

            // Build a fresh delegation engine with a cancel token so Ctrl+C
            // propagates into sub-agent SSE streams.
            let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
            let project_root = match std::env::current_dir() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "  {} Cannot determine working directory: {e}",
                        theme::icon_err()
                    );
                    return;
                }
            };
            let token = match crate::current_access_token(profile) {
                Some(t) => t,
                None => {
                    eprintln!("  {} Not logged in", theme::icon_err());
                    return;
                }
            };
            let (progress_tx, mut progress_rx) =
                tokio::sync::mpsc::unbounded_channel::<super::skill_subrun::SubRunProgressEvent>();

            // Start mock LLM server if --mock was requested
            let _mock_server;
            let effective_api = if let Some(scenario) = mock_scenario {
                match super::mock_llm::MockLlmServer::start(scenario).await {
                    Ok(srv) => {
                        let mock_api = match astra_thin_client::ThinClient::new(&srv.base_url, None)
                        {
                            Ok(a) => a,
                            Err(e) => {
                                eprintln!(
                                    "  {} Failed to create mock API client: {e}",
                                    theme::icon_err()
                                );
                                return;
                            }
                        };
                        _mock_server = Some(srv);
                        mock_api
                    }
                    Err(e) => {
                        eprintln!("  {} Failed to start mock server: {e}", theme::icon_err());
                        return;
                    }
                }
            } else {
                _mock_server = None;
                api.clone()
            };

            let executor = super::delegate_subrun::CliDelegateSubRunExecutor::new(
                effective_api,
                token,
                state.model.clone(),
                project_root.clone(),
                state.perm_manager.mode(),
                Some(cancel_token.clone()),
            )
            .with_progress_tx(progress_tx);
            let mut profile_registry = astra_services::coordination::AgentProfileRegistry::new();
            super::delegate_subrun::register_default_agents(&mut profile_registry);
            let _ = super::agent_loader::load_and_merge(&project_root, &mut profile_registry);
            let profile_registry = Arc::new(tokio::sync::RwLock::new(profile_registry));
            let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
            let run_engine = Arc::new(astra_runtime::server::run_engine::RunEngine::new(run_store));
            let tracker =
                Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
            let transport = Arc::new(astra_runtime::messaging::InProcessTransport::new());
            let mailbox_router = Arc::new(astra_runtime::messaging::AgentMailboxRouter::new(
                transport,
                tracker.clone(),
            ));
            let delegation_engine = Arc::new(
                astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
                    profile_registry.clone(),
                    run_engine.clone(),
                    tracker.clone(),
                    Arc::new(executor),
                )
                .with_gate(Arc::new(
                    astra_runtime::server::delegation_engine::DefaultQualityGate::default(),
                ))
                .with_mailbox_router(mailbox_router),
            );

            // Wire live progress callback for phase updates
            let progress: astra_runtime::server::team_orchestrator::ProgressCallback =
                Arc::new(move |phase: ExecutionPhase| match phase {
                    ExecutionPhase::Preparing {
                        team_name,
                        member_count,
                    } => {
                        eprintln!(
                            "  {} Preparing team '{}' ({} members)...",
                            "🔄".dim(),
                            team_name.cyan(),
                            member_count
                        );
                    }
                    ExecutionPhase::WorktreesCreated { ref agent_ids } => {
                        eprintln!(
                            "  {} Worktrees created for {} agent{}",
                            "📂".dim(),
                            agent_ids.len(),
                            if agent_ids.len() == 1 { "" } else { "s" }
                        );
                        for id in agent_ids {
                            eprintln!("    {} {}", "→".dim(), id.as_str().dim());
                        }
                    }
                    ExecutionPhase::Executing { ref delegation_id } => {
                        eprintln!(
                            "  {} Executing delegation {}...",
                            "🚀".dim(),
                            delegation_id.get(..8).unwrap_or(delegation_id).dim()
                        );
                    }
                    ExecutionPhase::Merging { agent_count } => {
                        eprintln!(
                            "  {} Merging results from {} agent{}...",
                            "🔀".dim(),
                            agent_count,
                            if agent_count == 1 { "" } else { "s" }
                        );
                    }
                    ExecutionPhase::Reporting { ref status } => {
                        eprintln!(
                            "  {} Generating report ({})...",
                            "📊".dim(),
                            status.to_string().dim()
                        );
                    }
                    ExecutionPhase::AgentProgress {
                        ref agent_states,
                        completed_count,
                        total_count,
                        ..
                    } => {
                        let summary: Vec<String> = agent_states
                            .iter()
                            .map(|(id, st)| format!("{}={}", id, st))
                            .collect();
                        eprintln!(
                            "  {} Progress: {}/{} agents done [{}]",
                            "📡".dim(),
                            completed_count,
                            total_count,
                            summary.join(", ").dim()
                        );
                    }
                });

            let config = astra_runtime::server::team_orchestrator::OrchestratorConfig {
                user_id: user_id.clone(),
                session_id,
                // Must match the profile registered by register_default_agents()
                source_agent_id: "main".to_string(),
                progress: Some(progress),
            };

            let orchestrator =
                astra_runtime::server::team_orchestrator::TeamExecutionOrchestrator::new(
                    team_store,
                    delegation_engine.clone(),
                    tracker.clone(),
                    run_engine,
                    profile_registry,
                    config,
                );

            // Print header
            eprintln!(
                "\n{}",
                format!("─── Team Run: {} ───", team_name).bold().cyan()
            );
            for m in &cli_team.members {
                eprintln!(
                    "    {} {} {}",
                    "→".dim(),
                    m.role.as_str().green(),
                    format!("— {}", m.description).dim()
                );
            }
            eprintln!("  {} {}\n", "📋 Task:".bold(), task);

            let started_at = chrono::Utc::now().to_rfc3339();
            let timer = Instant::now();

            // Progress renderer: 3s heartbeat with per-agent tracking and stall detection.
            let render_cancel = cancel_token.clone();
            let member_count = cli_team.members.len();
            let progress_renderer = tokio::spawn(async move {
                use astra_runtime::turn::agentic_headless_round::HeadlessStderrStyle;
                use std::collections::HashMap as ProgressMap;

                let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
                interval.tick().await; // skip first immediate tick
                let mut last_event_at = Instant::now();
                let mut agent_events: ProgressMap<String, u32> = ProgressMap::new();
                let mut ticker_dirty = false;

                loop {
                    tokio::select! {
                        biased;
                        Some(evt) = progress_rx.recv() => {
                            last_event_at = Instant::now();
                            if !evt.agent_id.is_empty() {
                                *agent_events.entry(evt.agent_id.clone()).or_insert(0) += 1;
                            }
                            if ticker_dirty {
                                eprint!("\r{}\r", " ".repeat(72));
                                ticker_dirty = false;
                            }
                            let tag = format_duration(timer.elapsed());
                            let prefix = if evt.agent_id.is_empty() {
                                format!("  [{}] ", tag.dim())
                            } else {
                                format!("  [{}] {} ", tag.dim(), evt.agent_id.cyan())
                            };
                            match evt.style {
                                HeadlessStderrStyle::Green =>
                                    eprintln!("{}{}", prefix, evt.line.green()),
                                HeadlessStderrStyle::Red =>
                                    eprintln!("{}{}", prefix, evt.line.red()),
                                HeadlessStderrStyle::Yellow =>
                                    eprintln!("{}{}", prefix, evt.line.yellow()),
                                _ =>
                                    eprintln!("{}{}", prefix, evt.line.dim()),
                            }
                        }
                        _ = interval.tick() => {
                            let elapsed = timer.elapsed();
                            let silence = last_event_at.elapsed();
                            let active = agent_events.len();
                            let status = if silence.as_secs() >= 30 {
                                format!(
                                    "running… {} | {}/{} agents | no output for {}",
                                    format_duration(elapsed), active, member_count,
                                    format_duration(silence),
                                )
                            } else {
                                format!(
                                    "running… {} | {}/{} agents active",
                                    format_duration(elapsed), active, member_count,
                                )
                            };
                            eprint!("\r{}\r  {} {}", " ".repeat(72), "⏳".dim(), status.dim());
                            ticker_dirty = true;
                        }
                        _ = render_cancel.cancelled() => {
                            if ticker_dirty {
                                eprint!("\r{}\r", " ".repeat(72));
                            }
                            break;
                        }
                    }
                }
            });

            let repo_root = std::env::current_dir().ok();

            // ─── Journal: record team execution start ────────────────────────
            let agent_roles: Vec<String> =
                cli_team.members.iter().map(|m| m.role.clone()).collect();
            let coordination_label = cli_team
                .coordination
                .as_ref()
                .map(|c| format!("{c:?}"))
                .unwrap_or_else(|| "auto".to_string());
            if let Some(ref j) = state.journal {
                let _ = j.append(
                    &astra_services::session_journal::JournalEvent::delegation_started(
                        state.session_id.as_deref(),
                        team_name, // delegation_id not yet known — use team name
                        "orchestrator",
                        &coordination_label,
                        &agent_roles,
                    ),
                );
            }

            let report = tokio::select! {
                report = orchestrator.execute_team(team_name, task, repo_root) => report,
                _ = tokio::signal::ctrl_c() => {
                    cancel_token.cancel();
                    eprint!("\r{}\r", " ".repeat(72));
                    eprintln!("  {} Interrupting team run...", "⚠️ ".yellow());
                    // Give sub-runs a moment to notice cancellation
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    eprintln!("  {} Team run interrupted.", theme::icon_err());
                    progress_renderer.abort();
                    // Journal: record interrupted execution
                    if let Some(ref j) = state.journal {
                        let _ = j.append(
                            &astra_services::session_journal::JournalEvent::delegation_completed(
                                state.session_id.as_deref(),
                                team_name,
                                &coordination_label,
                                cli_team.members.len(),
                                0,
                                0,
                                "interrupted",
                                None,
                            ),
                        );
                    }
                    return;
                }
            };
            cancel_token.cancel();
            progress_renderer.abort();
            let elapsed = timer.elapsed();

            // Display report header with status
            eprintln!();
            match report.status {
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::Completed => {
                    eprintln!(
                        "  ✅ Team '{}' completed successfully {}",
                        team_name.green().bold(),
                        format!("({})", format_duration(elapsed)).dim()
                    );
                }
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::Partial => {
                    eprintln!(
                        "  ⚠️  Team '{}' completed partially {}",
                        team_name.yellow().bold(),
                        format!("({})", format_duration(elapsed)).dim()
                    );
                    if let Some(ref err) = report.error {
                        eprintln!("    {} {}", theme::icon_warn(), err.as_str().yellow());
                    }
                }
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::CompletedWithConflicts => {
                    eprintln!(
                        "  {}  Team '{}' completed with merge conflicts {}",
                        theme::icon_warn(), team_name.yellow().bold(),
                        format!("({})", format_duration(elapsed)).dim()
                    );
                    if let Some(ref err) = report.error {
                        eprintln!("    {} {}", theme::icon_warn(), err.as_str().yellow());
                    }
                }
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::CompletedOverBudget => {
                    eprintln!(
                        "  {}  Team '{}' completed over budget {}",
                        theme::icon_warn(), team_name.yellow().bold(),
                        format!("({})", format_duration(elapsed)).dim()
                    );
                    if let Some(ref err) = report.error {
                        eprintln!("    {} {}", theme::icon_warn(), err.as_str().yellow());
                    }
                }
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::Failed => {
                    eprintln!(
                        "  {} Team '{}' execution failed {}",
                        theme::icon_err(), team_name.red().bold(),
                        format!("({})", format_duration(elapsed)).dim()
                    );
                    if let Some(ref err) = report.error {
                        eprintln!("    {} {}", theme::icon_err(), err);
                    }
                }
            }

            // Agent results
            if let Some(ref dr) = report.delegation_result {
                let total_tokens = dr.total_prompt_tokens + dr.total_completion_tokens;
                eprintln!(
                    "\n  📊 {} agent{} | {} tokens ({}↑ {}↓) | delegation {}",
                    dr.agent_results.len(),
                    if dr.agent_results.len() == 1 { "" } else { "s" },
                    format_tokens(total_tokens),
                    format_tokens(dr.total_prompt_tokens),
                    format_tokens(dr.total_completion_tokens),
                    dr.delegation_id.get(..8).unwrap_or(&dr.delegation_id).dim(),
                );
                for ar in &dr.agent_results {
                    let is_success = ar.status == "completed" && ar.error.is_none();
                    let (status_icon, status_color) = if is_success {
                        ("✓", "green")
                    } else {
                        ("✗", "red")
                    };
                    let first_line = ar
                        .output
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .or_else(|| ar.error.as_deref().filter(|value| !value.trim().is_empty()))
                        .unwrap_or(ar.status.as_str())
                        .lines()
                        .next()
                        .unwrap_or("");
                    let agent_tokens = ar.prompt_tokens + ar.completion_tokens;
                    let status_label = if is_success {
                        format!("({}tok)", format_tokens(agent_tokens))
                    } else {
                        format!("({} · {}tok)", ar.status, format_tokens(agent_tokens))
                    };
                    match status_color {
                        "green" => eprintln!(
                            "    {} {} {} — {}",
                            status_icon.green(),
                            ar.agent_id.as_str().cyan(),
                            status_label.dim(),
                            truncate_str(first_line, 72),
                        ),
                        _ => eprintln!(
                            "    {} {} {} — {}",
                            status_icon.red(),
                            ar.agent_id.as_str().cyan(),
                            status_label.dim(),
                            truncate_str(first_line, 72),
                        ),
                    }
                }
            }

            // Merge results
            if let Some(ref merge) = report.merge_result {
                if merge.conflicts.is_empty() {
                    if !merge.merged.is_empty() {
                        eprintln!(
                            "\n  🔀 Merge: {} branch{} merged cleanly",
                            merge.merged.len(),
                            if merge.merged.len() == 1 { "" } else { "es" }
                        );
                    }
                } else {
                    eprintln!(
                        "\n  🔀 Merge: {} conflict{}",
                        merge.conflicts.len(),
                        if merge.conflicts.len() == 1 { "" } else { "s" }
                    );
                    for c in &merge.conflicts {
                        eprintln!(
                            "    {} {} — {}",
                            "!".red().bold(),
                            c.agent_id.as_str().yellow(),
                            c.files.join(", ")
                        );
                    }
                }
            }

            // Learning summary
            if let Some(ref learning) = report.merged_learning {
                let has_patterns = !learning.consensus_patterns.is_empty();
                let has_facts = !learning.facts.is_empty();
                let has_caution = !learning.cautionary_patterns.is_empty();
                if has_patterns || has_facts || has_caution {
                    eprintln!(
                        "\n  🧠 Learning from {} agent{}:",
                        learning.agent_count,
                        if learning.agent_count == 1 { "" } else { "s" }
                    );
                    if has_patterns {
                        eprintln!(
                            "    {} {} consensus pattern{}",
                            "•".dim(),
                            learning.consensus_patterns.len(),
                            if learning.consensus_patterns.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        );
                    }
                    if has_facts {
                        eprintln!(
                            "    {} {} discovered fact{}",
                            "•".dim(),
                            learning.facts.len(),
                            if learning.facts.len() == 1 { "" } else { "s" }
                        );
                        for fact in learning.facts.iter().take(3) {
                            eprintln!("      {} {}", "→".dim(), truncate_str(fact, 70).dim());
                        }
                        if learning.facts.len() > 3 {
                            eprintln!(
                                "      {} ...and {} more",
                                "→".dim(),
                                learning.facts.len() - 3
                            );
                        }
                    }
                    if has_caution {
                        eprintln!(
                            "    {} {} cautionary pattern{}",
                            "⚡".dim(),
                            learning.cautionary_patterns.len(),
                            if learning.cautionary_patterns.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        );
                    }
                }
            }
            eprintln!();

            // Record this execution in history (in-memory + persistence service)
            let (prompt_tok, compl_tok, agent_count) = report
                .delegation_result
                .as_ref()
                .map(|dr| {
                    (
                        dr.total_prompt_tokens,
                        dr.total_completion_tokens,
                        dr.agent_results.len(),
                    )
                })
                .unwrap_or((0, 0, 0));
            state.team_registry.record_execution(TeamHistoryEntry {
                team_name: team_name.to_string(),
                task: task.to_string(),
                delegation_id: report.delegation_id.clone(),
                parent_run_id: report.parent_run_id.clone(),
                status: report.status.to_string(),
                agent_count,
                total_prompt_tokens: prompt_tok,
                total_completion_tokens: compl_tok,
                error: report.error.clone(),
                started_at,
            });

            // ─── Journal: record team execution completion ───────────────
            if let Some(ref j) = state.journal {
                let (succeeded, failed) = report
                    .delegation_result
                    .as_ref()
                    .map(|dr| {
                        let s = dr
                            .agent_results
                            .iter()
                            .filter(|r| r.status == "completed" && r.error.is_none())
                            .count();
                        (s, dr.agent_results.len() - s)
                    })
                    .unwrap_or((0, 0));
                let _ = j.append(
                    &astra_services::session_journal::JournalEvent::delegation_completed(
                        state.session_id.as_deref(),
                        &report.delegation_id,
                        &coordination_label,
                        agent_count,
                        succeeded,
                        failed,
                        &report.status.to_string(),
                        report
                            .delegation_result
                            .as_ref()
                            .and_then(|result| result.aggregated_output.as_deref()),
                    ),
                );
            }
        }

        "history" => {
            // /team history <team>
            // Currently reads from in-memory TeamRegistry. When MatrixOne backend is active,
            // state.team_store.list_executions() becomes the primary source.
            let name = sub_arg.trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /team history <team>".yellow());
                eprintln!("{}", "  Shows execution history for a team.".dim());
                return;
            }
            let team = match state.team_registry.get(name) {
                Some(t) => t.clone(),
                None => {
                    eprintln!("  {} Team '{}' not found", theme::icon_err(), name);
                    return;
                }
            };

            // Query persistence store using stable team_id (not display name)
            let store_entries = state
                .team_store
                .list_executions(&team.team_id, 50)
                .await
                .unwrap_or_default();
            let registry_entries = state.team_registry.get_history(name);

            if registry_entries.is_empty() && store_entries.is_empty() {
                eprintln!(
                    "\n  📜 No execution history for team '{}'.",
                    name.cyan().bold()
                );
                eprintln!("  {}", "  Use /team run to execute a task.".dim());
                return;
            }

            // Display from in-memory registry (primary for this session)
            let entries = &registry_entries;
            let store_extra = if store_entries.len() > entries.len() {
                store_entries.len() - entries.len()
            } else {
                0
            };

            eprintln!(
                "\n{}",
                format!(
                    "─── History: {} ({} run{}{}) ───",
                    name,
                    entries.len(),
                    if entries.len() == 1 { "" } else { "s" },
                    if store_extra > 0 {
                        format!(", +{} in store", store_extra)
                    } else {
                        String::new()
                    }
                )
                .bold()
            );
            for (i, e) in entries.iter().enumerate().rev() {
                let status_icon = match e.status.as_str() {
                    "completed" => "✅",
                    "partial" => "⚠️ ",
                    "completed_with_conflicts" => "⚠️ ",
                    _ => "❌",
                };
                eprintln!(
                    "\n  {} #{} {} {}",
                    status_icon,
                    i + 1,
                    e.status.as_str().bold(),
                    e.started_at.as_str().dim()
                );
                eprintln!("    {} {}", "Task:".dim(), truncate_str(&e.task, 70));
                eprintln!(
                    "    {} agents: {} | tokens: {} ({}↑ {}↓) | delegation: {}",
                    "📊".dim(),
                    e.agent_count,
                    format_tokens(e.total_prompt_tokens + e.total_completion_tokens),
                    format_tokens(e.total_prompt_tokens),
                    format_tokens(e.total_completion_tokens),
                    e.delegation_id.get(..8).unwrap_or(&e.delegation_id),
                );
                if let Some(ref err) = e.error {
                    eprintln!("    {} {}", theme::icon_err(), truncate_str(err, 60));
                }
            }
            eprintln!();
        }

        "snapshot" => {
            // /team snapshot <team> [label]
            let mut parts = sub_arg.splitn(2, ' ');
            let name = parts.next().unwrap_or("").trim();
            let label = parts.next().unwrap_or("").trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /team snapshot <team> [label]".yellow());
                eprintln!("{}", "  Saves team state + git commit as a snapshot.".dim());
                return;
            }
            if state.team_registry.get(name).is_none() {
                eprintln!("  {} Team '{}' not found", theme::icon_err(), name);
                return;
            }

            let snapshot_id = format!("team-{}-{}", name, chrono::Utc::now().timestamp());
            let git_sha = git_head_sha();
            let session_id = state.session_id.clone();
            let now = chrono::Utc::now().to_rfc3339();

            let snap_label = if label.is_empty() {
                format!("team {} snapshot", name)
            } else {
                label.to_string()
            };

            // Build CompositeSnapshot
            let mut builder = astra_core::composite_snapshot::CompositeSnapshotBuilder::new(
                session_id.clone().unwrap_or_default(),
                0,
            )
            .label(&snap_label);

            if let Some(ref sha) = git_sha {
                builder = builder.git_commit(sha);
            }
            if let Some(ref sid) = session_id {
                builder = builder.session_state(sid);
            }

            let composite = builder.build();

            state.team_registry.add_snapshot(TeamSnapshotEntry {
                snapshot_id: snapshot_id.clone(),
                team_name: name.to_string(),
                label: snap_label.clone(),
                git_commit: git_sha.clone(),
                session_id: session_id.clone(),
                created_at: now.clone(),
            });

            // Persist snapshot to store (with full team definition JSON)
            let team_def_json = state.team_registry.get(name).map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "members": t.members.iter().map(|m| serde_json::json!({
                        "role": m.role,
                        "description": m.description,
                        "skills": m.skills,
                        "model_override": m.model_override,
                    })).collect::<Vec<_>>(),
                    "shared_context": t.shared_context,
                })
                .to_string()
            });
            let snap_record = astra_services::team_persistence::TeamSnapshotRecord {
                snapshot_id: snapshot_id.clone(),
                team_name: name.to_string(),
                user_id: String::new(),
                label: snap_label.clone(),
                git_commit: git_sha.clone(),
                session_id,
                team_definition_json: team_def_json,
                created_at: now,
            };
            let _ = state.team_store.save_snapshot(&snap_record).await;

            eprintln!(
                "\n  {} Snapshot '{}' created for team '{}'",
                theme::icon_ok(),
                snapshot_id.as_str().dim(),
                name.cyan()
            );
            let dims = composite.dimensions();
            eprintln!("    {} Captured: {}", "📸".dim(), dims.join(", "),);
            if let Some(ref sha) = git_sha {
                eprintln!("    {} Git: {}", "🔖".dim(), sha.get(..12).unwrap_or(sha),);
            }
            eprintln!(
                "    {} Use '/team restore {} {}' to restore.",
                "💡".dim(),
                name,
                &snapshot_id,
            );
            eprintln!();
        }

        "restore" => {
            // /team restore <team> <snapshot-id>
            let mut parts = sub_arg.splitn(2, ' ');
            let name = parts.next().unwrap_or("").trim();
            let snapshot_id = parts.next().unwrap_or("").trim();
            if name.is_empty() || snapshot_id.is_empty() {
                eprintln!("{}", "  Usage: /team restore <team> <snapshot-id>".yellow());
                eprintln!("{}", "  Restores git state from a team snapshot.".dim());
                return;
            }
            if state.team_registry.get(name).is_none() {
                eprintln!("  {} Team '{}' not found", theme::icon_err(), name);
                return;
            }

            let snap = match state.team_registry.find_snapshot(snapshot_id) {
                Some(s) => s.clone(),
                None => {
                    // Try prefix match in registry
                    let matches: Vec<_> = state
                        .team_registry
                        .get_snapshots(name)
                        .into_iter()
                        .filter(|s| s.snapshot_id.starts_with(snapshot_id))
                        .collect();
                    match matches.len() {
                        0 => {
                            // Fallback: try persistence store
                            if let Ok(Some(stored)) =
                                state.team_store.find_snapshot(snapshot_id, "").await
                            {
                                TeamSnapshotEntry {
                                    snapshot_id: stored.snapshot_id,
                                    team_name: stored.team_name,
                                    label: stored.label,
                                    git_commit: stored.git_commit,
                                    session_id: stored.session_id,
                                    created_at: stored.created_at,
                                }
                            } else {
                                eprintln!(
                                    "  {} Snapshot '{}' not found",
                                    theme::icon_err(),
                                    snapshot_id
                                );
                                let available = state.team_registry.get_snapshots(name);
                                if !available.is_empty() {
                                    eprintln!("  {} Available snapshots:", "💡".dim());
                                    for s in available {
                                        eprintln!(
                                            "    {} {} — {}",
                                            "•".dim(),
                                            s.snapshot_id.as_str().dim(),
                                            s.label
                                        );
                                    }
                                }
                                return;
                            }
                        }
                        1 => matches[0].clone(),
                        _ => {
                            eprintln!(
                                "  {} Ambiguous snapshot prefix '{}'. Matches:",
                                theme::icon_err(),
                                snapshot_id
                            );
                            for s in matches {
                                eprintln!("    {} {}", "•".dim(), s.snapshot_id);
                            }
                            return;
                        }
                    }
                }
            };

            if snap.team_name != name {
                eprintln!(
                    "  {} Snapshot '{}' belongs to team '{}', not '{}'",
                    theme::icon_err(),
                    snap.snapshot_id,
                    snap.team_name,
                    name
                );
                return;
            }

            // Safety: check for uncommitted changes before restore
            let git_dirty = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .output()
                .ok()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false);
            if git_dirty {
                eprintln!(
                    "  {} Working tree has uncommitted changes. Commit or stash first.",
                    theme::icon_err()
                );
                eprintln!(
                    "  {}",
                    "  Run `git stash` to save changes, then retry.".dim()
                );
                return;
            }

            eprintln!(
                "\n  ⏪ Restoring team '{}' from snapshot '{}'...",
                name.cyan(),
                snap.snapshot_id.dim()
            );
            eprintln!("    {} {}", "Label:".dim(), snap.label);

            // Restore git state if snapshot has a commit
            if let Some(ref sha) = snap.git_commit {
                eprintln!(
                    "    {} Checking out git commit {}...",
                    "🔖".dim(),
                    sha.get(..12).unwrap_or(sha)
                );
                let output = std::process::Command::new("git")
                    .args(["checkout", sha])
                    .output();
                match output {
                    Ok(o) if o.status.success() => {
                        eprintln!(
                            "    {} Git restored to {}",
                            theme::icon_ok(),
                            sha.get(..12).unwrap_or(sha)
                        );
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        eprintln!(
                            "    {} Git checkout failed: {}",
                            theme::icon_err(),
                            truncate_str(stderr.trim(), 60)
                        );
                    }
                    Err(e) => {
                        eprintln!("    {} Git checkout error: {}", theme::icon_err(), e);
                    }
                }
            } else {
                eprintln!(
                    "    {} No git commit in snapshot (git state unchanged)",
                    "ℹ️ ".dim()
                );
            }

            eprintln!("  {} Restore complete.\n", theme::icon_ok());
        }

        "status" => {
            let name = sub_arg.trim();
            let entries: Vec<TeamHistoryEntry> = if name.is_empty() {
                let mut all: Vec<_> = state.team_registry.history.to_vec();
                all.sort_by(|a, b| b.started_at.cmp(&a.started_at));
                all.into_iter().take(5).collect()
            } else {
                state
                    .team_registry
                    .get_history(name)
                    .into_iter()
                    .rev()
                    .take(3)
                    .cloned()
                    .collect()
            };
            if entries.is_empty() {
                eprintln!("  {} No recent team executions.", "ℹ️ ".dim());
                return;
            }
            eprintln!("\n{}", "─── Recent Team Runs ───".bold().cyan());
            for e in &entries {
                let icon = match e.status.as_str() {
                    "completed" => "✅",
                    "partial" | "completed_with_conflicts" => "⚠️ ",
                    _ => "❌",
                };
                eprintln!(
                    "  {} {} {} — {} ({} agents, {}tok)",
                    icon,
                    e.team_name.as_str().cyan(),
                    e.status.as_str().dim(),
                    truncate_str(&e.task, 50),
                    e.agent_count,
                    format_tokens(e.total_prompt_tokens + e.total_completion_tokens),
                );
                eprintln!(
                    "    {} delegation: {}",
                    "→".dim(),
                    e.delegation_id.get(..12).unwrap_or(&e.delegation_id).dim()
                );
            }
            eprintln!();
        }

        "agents" => {
            // /team agents — list all available agent types (builtin + custom)
            let spawner = match state.agent_spawner.as_ref() {
                Some(s) => s,
                None => {
                    eprintln!("  {} Agent spawner not initialized", theme::icon_err());
                    return;
                }
            };
            let registry = spawner.agent_registry();
            let all = registry.list_all();
            let builtin_count = 4; // explore, task, code-review, general-purpose

            eprintln!(
                "\n{}",
                "─── Agent Types ─────────────────────────────────────────"
                    .bold()
                    .cyan()
            );
            for def in all.iter() {
                let tag = if registry.is_custom(&def.agent_type) {
                    " (custom)".yellow().to_string()
                } else {
                    " (builtin)".dim().to_string()
                };
                eprintln!("\n  {}{}", def.agent_type.as_str().cyan().bold(), tag,);
                eprintln!("    {}", def.description.as_str().dim());
                eprintln!(
                    "    {} {} | {} {} | {}",
                    "Model:".dim(),
                    def.default_model.as_str(),
                    "Max turns:".dim(),
                    def.max_turns,
                    if def.read_only {
                        "read-only".yellow().to_string()
                    } else {
                        "read-write".green().to_string()
                    }
                );
            }
            let custom_count = all.len().saturating_sub(builtin_count);
            eprintln!(
                "\n  {} builtin, {} custom\n",
                builtin_count.min(all.len()),
                custom_count
            );
        }

        _ => {
            eprintln!(
                "{}",
                format!(
                    "  Unknown /team subcommand: '{sub}'. {}",
                    team_subcommands_hint()
                )
                .yellow()
            );
        }
    }
}

fn team_subcommands_hint() -> &'static str {
    "Subcommands: /team list · info · create · add-member · context · run · status · history · snapshot · restore · delete · agents · help"
}

// ─── Formatting helpers ────────────────────────────────────────────────────

/// Format a Duration as a human-readable string (e.g. "2.3s", "1m 12s").
fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    if secs >= 60 {
        let mins = secs / 60;
        let rem = secs % 60;
        format!("{mins}m {rem}s")
    } else if secs >= 10 {
        format!("{secs}s")
    } else {
        format!("{secs}.{:01}s", millis / 100)
    }
}

/// Format token counts with K/M suffixes for readability.
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_utils::{CredentialsFile, Profile};
    use axum::{Router, routing::get, routing::post};

    async fn spawn_mock(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::task::yield_now().await;
        base
    }

    #[test]
    fn registry_has_builtin_teams() {
        let reg = TeamRegistry::new();
        assert!(reg.get("review").is_some());
        assert!(reg.get("research").is_some());
        assert!(reg.get("dev").is_some());
        assert_eq!(reg.list().len(), 3);
    }

    #[test]
    fn create_and_delete_team() {
        let mut reg = TeamRegistry::new();
        reg.create("my-team".into(), "test team".into(), None)
            .unwrap();
        assert!(reg.get("my-team").is_some());
        assert_eq!(reg.list().len(), 4);

        reg.remove("my-team").unwrap();
        assert!(reg.get("my-team").is_none());
    }

    #[test]
    fn create_duplicate_fails() {
        let mut reg = TeamRegistry::new();
        assert!(reg.create("review".into(), "dup".into(), None).is_err());
    }

    #[test]
    fn add_member_to_team() {
        let mut reg = TeamRegistry::new();
        reg.create("test".into(), "test".into(), None).unwrap();
        reg.add_member(
            "test",
            TeamMember {
                role: "coder".into(),
                description: "writes code".into(),
                skills: vec![],
                model_override: None,
            },
        )
        .unwrap();
        let team = reg.get("test").unwrap();
        assert_eq!(team.members.len(), 1);
        assert_eq!(team.members[0].role, "coder");
    }

    #[test]
    fn add_duplicate_role_fails() {
        let mut reg = TeamRegistry::new();
        reg.create("test".into(), "test".into(), None).unwrap();
        let member = TeamMember {
            role: "coder".into(),
            description: "v1".into(),
            skills: vec![],
            model_override: None,
        };
        reg.add_member("test", member.clone()).unwrap();
        assert!(reg.add_member("test", member).is_err());
    }

    #[test]
    fn shared_context_set_and_read() {
        let mut reg = TeamRegistry::new();
        reg.set_context("review", "project".into(), "astra".into())
            .unwrap();
        let team = reg.get("review").unwrap();
        assert_eq!(team.shared_context.get("project").unwrap(), "astra");
    }

    #[test]
    fn remove_nonexistent_fails() {
        let mut reg = TeamRegistry::new();
        assert!(reg.remove("ghost").is_err());
    }

    // ── Coordination inference tests ────────────────────────────────

    fn make_team(roles: &[&str]) -> Team {
        Team {
            team_id: uuid::Uuid::new_v4().to_string(),
            name: "test".into(),
            description: "test team".into(),
            members: roles
                .iter()
                .map(|r| TeamMember {
                    role: r.to_string(),
                    description: format!("{r} agent"),
                    skills: vec![],
                    model_override: None,
                })
                .collect(),
            shared_context: HashMap::new(),
            worktree_mode: WorktreeMode::Shared,
            coordination: None,
            created_at: "2024-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn infer_adversarial_from_producer_reviewer() {
        use astra_services::team_persistence::TeamCoordination;
        let team = make_team(&["producer", "reviewer"]);
        let coord = infer_coordination(&team);
        assert!(matches!(coord, TeamCoordination::Adversarial { .. }));
    }

    #[test]
    fn infer_adversarial_from_writer_critic() {
        use astra_services::team_persistence::TeamCoordination;
        let team = make_team(&["writer", "critic"]);
        let coord = infer_coordination(&team);
        assert!(matches!(coord, TeamCoordination::Adversarial { .. }));
    }

    #[test]
    fn infer_pipeline_from_analyst_implementer() {
        use astra_services::team_persistence::TeamCoordination;
        let team = make_team(&["analyst", "implementer"]);
        let coord = infer_coordination(&team);
        assert!(matches!(coord, TeamCoordination::Pipeline));
    }

    #[test]
    fn infer_pipeline_from_explorer_synthesizer() {
        use astra_services::team_persistence::TeamCoordination;
        let team = make_team(&["explorer", "synthesizer"]);
        let coord = infer_coordination(&team);
        assert!(matches!(coord, TeamCoordination::Pipeline));
    }

    #[test]
    fn infer_fanout_for_generic_roles() {
        use astra_services::team_persistence::TeamCoordination;
        let team = make_team(&["alpha", "beta", "gamma"]);
        let coord = infer_coordination(&team);
        assert!(matches!(coord, TeamCoordination::FanOut { .. }));
    }

    #[test]
    fn cli_team_to_definition_converts_members() {
        let team = make_team(&["coder", "tester"]);
        let def = cli_team_to_definition(&team, "user-123");
        assert_eq!(def.name, "test");
        assert_eq!(def.user_id, "user-123");
        assert_eq!(def.members.len(), 2);
        assert_eq!(def.members[0].role, "coder");
        assert_eq!(def.members[1].role, "tester");
        assert!(def.members[0].agent_id.is_none());
        assert_eq!(def.members[0].system_prompt.as_deref(), Some("coder agent"));
    }

    #[test]
    fn cli_team_to_definition_preserves_context() {
        let mut team = make_team(&["a"]);
        team.shared_context.insert("lang".into(), "rust".into());
        let def = cli_team_to_definition(&team, "u");
        assert_eq!(def.context.get("lang").unwrap(), "rust");
    }

    #[test]
    fn cli_team_to_definition_preserves_worktree_mode() {
        let mut reg = TeamRegistry::new();
        let dev = reg.get("dev").unwrap().clone();
        let def = cli_team_to_definition(&dev, "u");
        assert_eq!(def.worktree_mode, WorktreeMode::Isolated);

        reg.create("custom".into(), "custom".into(), None).unwrap();
        let custom = reg.get("custom").unwrap().clone();
        let custom_def = cli_team_to_definition(&custom, "u");
        assert_eq!(custom_def.worktree_mode, WorktreeMode::Shared);
    }

    // ── History / Snapshot tests ─────────────────────────────────

    #[test]
    fn record_and_get_history() {
        let mut reg = TeamRegistry::new();
        assert!(reg.get_history("dev").is_empty());

        reg.record_execution(TeamHistoryEntry {
            team_name: "dev".into(),
            task: "fix auth".into(),
            delegation_id: "deleg-1".into(),
            parent_run_id: "run-1".into(),
            status: "completed".into(),
            agent_count: 3,
            total_prompt_tokens: 1000,
            total_completion_tokens: 500,
            error: None,
            started_at: "2025-01-01T00:00:00Z".into(),
        });
        reg.record_execution(TeamHistoryEntry {
            team_name: "dev".into(),
            task: "add tests".into(),
            delegation_id: "deleg-2".into(),
            parent_run_id: "run-2".into(),
            status: "failed".into(),
            agent_count: 2,
            total_prompt_tokens: 600,
            total_completion_tokens: 200,
            error: Some("timeout".into()),
            started_at: "2025-01-02T00:00:00Z".into(),
        });

        let history = reg.get_history("dev");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].task, "fix auth");
        assert_eq!(history[1].status, "failed");
        assert_eq!(history[1].error.as_deref(), Some("timeout"));

        // Different team has no history
        assert!(reg.get_history("review").is_empty());
    }

    #[test]
    fn add_and_find_snapshot() {
        let mut reg = TeamRegistry::new();
        reg.add_snapshot(TeamSnapshotEntry {
            snapshot_id: "snap-abc123".into(),
            team_name: "dev".into(),
            label: "before refactor".into(),
            git_commit: Some("deadbeef".into()),
            session_id: Some("sess-1".into()),
            created_at: "2025-01-01T00:00:00Z".into(),
        });
        reg.add_snapshot(TeamSnapshotEntry {
            snapshot_id: "snap-def456".into(),
            team_name: "review".into(),
            label: "baseline".into(),
            git_commit: None,
            session_id: None,
            created_at: "2025-01-02T00:00:00Z".into(),
        });

        // Exact match
        let found = reg.find_snapshot("snap-abc123");
        assert!(found.is_some());
        assert_eq!(found.unwrap().label, "before refactor");

        // Non-existent
        let found = reg.find_snapshot("snap-xyz");
        assert!(found.is_none());

        // Team-scoped list
        assert_eq!(reg.get_snapshots("dev").len(), 1);
        assert_eq!(reg.get_snapshots("review").len(), 1);
        assert_eq!(reg.get_snapshots("research").len(), 0);
    }

    #[test]
    fn git_head_sha_returns_some_in_git_repo() {
        // This test runs inside a git repo, so should return Some
        let sha = git_head_sha();
        assert!(sha.is_some(), "Expected Some(sha) in a git repo");
        let sha = sha.unwrap();
        assert!(sha.len() >= 7, "SHA too short: {}", sha);
    }

    #[test]
    fn team_subcommands_hint_mentions_run_and_restore() {
        let hint = team_subcommands_hint();
        assert!(hint.contains("run"));
        assert!(hint.contains("restore"));
        assert!(hint.contains("help"));
    }

    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Serialize tests that mutate ASTRA_CREDENTIALS_DIR.
    fn creds_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    async fn ensure_team_run_session_creates_remote_session_when_missing() {
        let _lock = creds_lock();
        let creds_dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("ASTRA_CREDENTIALS_DIR", creds_dir.path());
        }

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("team-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let app = Router::new().route(
            "/sessions",
            post(|| async { axum::Json(serde_json::json!({ "session_id": "team-sess-1" })) }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut state = ReplState::default();

        let session_id = ensure_team_run_session(&api, None, &mut state)
            .await
            .unwrap();

        assert_eq!(session_id, "team-sess-1");
        assert_eq!(state.session_id.as_deref(), Some("team-sess-1"));
        assert!(state.journal.is_some());

        let creds = load_credentials();
        assert_eq!(
            creds.profiles["default"].last_session_id.as_deref(),
            Some("team-sess-1")
        );
    }

    #[tokio::test]
    async fn ensure_team_run_session_replaces_stale_remote_session() {
        let _lock = creds_lock();
        let creds_dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("ASTRA_CREDENTIALS_DIR", creds_dir.path());
        }

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("team-token".to_string()),
                last_session_id: Some("stale-sess".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let app = Router::new()
            .route(
                "/sessions/{id}",
                get(|| async {
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        axum::Json(serde_json::json!({ "detail": "Session not found" })),
                    )
                }),
            )
            .route(
                "/sessions",
                post(|| async { axum::Json(serde_json::json!({ "session_id": "team-sess-2" })) }),
            );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut state = ReplState {
            session_id: Some("stale-sess".to_string()),
            journal: session_journal::JournalWriter::new("stale-sess").ok(),
            ..Default::default()
        };

        let session_id = ensure_team_run_session(&api, None, &mut state)
            .await
            .unwrap();

        assert_eq!(session_id, "team-sess-2");
        assert_eq!(state.session_id.as_deref(), Some("team-sess-2"));
        assert!(state.journal.is_some());

        let creds = load_credentials();
        assert_eq!(
            creds.profiles["default"].last_session_id.as_deref(),
            Some("team-sess-2")
        );
    }

    // ── Formatting helper tests ─────────────────────────────────

    #[test]
    fn format_duration_sub_second() {
        let d = std::time::Duration::from_millis(500);
        assert_eq!(format_duration(d), "0.5s");
    }

    #[test]
    fn format_duration_seconds() {
        let d = std::time::Duration::from_secs(3) + std::time::Duration::from_millis(200);
        assert_eq!(format_duration(d), "3.2s");
    }

    #[test]
    fn format_duration_ten_plus_seconds() {
        let d = std::time::Duration::from_secs(42);
        assert_eq!(format_duration(d), "42s");
    }

    #[test]
    fn format_duration_minutes() {
        let d = std::time::Duration::from_secs(125);
        assert_eq!(format_duration(d), "2m 5s");
    }

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(9999), "9999");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(10000), "10.0K");
        assert_eq!(format_tokens(42500), "42.5K");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(1_500_000), "1.5M");
        assert_eq!(format_tokens(2_000_000), "2.0M");
    }

    // ── Progress callback type check ────────────────────────────

    #[test]
    fn execution_phase_display() {
        // Verify the ExecutionPhase variants we use in the progress callback
        let p = ExecutionPhase::Preparing {
            team_name: "dev".into(),
            member_count: 3,
        };
        assert!(matches!(p, ExecutionPhase::Preparing { .. }));

        let p = ExecutionPhase::Executing {
            delegation_id: "abc123".into(),
        };
        assert!(matches!(p, ExecutionPhase::Executing { .. }));
    }

    // ── New feature tests ───────────────────────────────────────

    #[test]
    fn create_with_explicit_coordination() {
        use astra_services::team_persistence::TeamCoordination;
        let mut reg = TeamRegistry::new();
        let coord = Some(TeamCoordination::Pipeline);
        reg.create("pipe-team".into(), "pipeline team".into(), coord)
            .unwrap();
        let team = reg.get("pipe-team").unwrap();
        assert!(matches!(
            team.coordination,
            Some(TeamCoordination::Pipeline)
        ));
    }

    #[test]
    fn explicit_coordination_overrides_inference() {
        use astra_services::team_persistence::TeamCoordination;
        let mut team = make_team(&["producer", "reviewer"]);
        // Without explicit coordination, infer_coordination returns Adversarial
        assert!(matches!(
            infer_coordination(&team),
            TeamCoordination::Adversarial { .. }
        ));
        // With explicit Pipeline, cli_team_to_definition should use Pipeline
        team.coordination = Some(TeamCoordination::Pipeline);
        let def = cli_team_to_definition(&team, "u");
        assert!(matches!(def.coordination, TeamCoordination::Pipeline));
    }

    #[test]
    fn merge_from_store_skips_existing() {
        use astra_services::team_persistence::*;
        let mut reg = TeamRegistry::new();
        assert!(reg.get("review").is_some()); // builtin

        let foreign = TeamDefinition {
            team_id: "foreign-id".into(),
            user_id: "u".into(),
            name: "review".into(), // same name as builtin
            description: "foreign review".into(),
            coordination: TeamCoordination::Pipeline,
            members: vec![],
            context: HashMap::new(),
            worktree_mode: WorktreeMode::Shared,
            budget: None,
            max_parallel: 0,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        };
        let custom = TeamDefinition {
            team_id: "custom-id".into(),
            user_id: "u".into(),
            name: "from-store".into(),
            description: "loaded from store".into(),
            coordination: TeamCoordination::FanOut {
                aggregation: "merge".into(),
            },
            members: vec![TeamMemberDef {
                role: "worker".into(),
                agent_id: None,
                system_prompt: Some("does work".into()),
                skills: vec![],
                model_override: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
            }],
            context: HashMap::new(),
            worktree_mode: WorktreeMode::Shared,
            budget: None,
            max_parallel: 0,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        };

        reg.merge_from_store(vec![foreign, custom]);

        // "review" should NOT be overwritten
        let review = reg.get("review").unwrap();
        assert_ne!(review.team_id, "foreign-id");

        // "from-store" should be loaded
        let loaded = reg.get("from-store").unwrap();
        assert_eq!(loaded.team_id, "custom-id");
        assert_eq!(loaded.members.len(), 1);
        assert_eq!(loaded.members[0].description, "does work");
    }

    #[test]
    fn store_loaded_flag_default_false() {
        let reg = TeamRegistry::new();
        assert!(!reg.store_loaded);
    }
}
