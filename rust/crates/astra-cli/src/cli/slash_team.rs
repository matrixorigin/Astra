use super::*;
use astra_runtime::server::team_orchestrator::ExecutionPhase;
#[allow(unused_imports)]
use astra_services::team_persistence::TeamPersistenceService;
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
    pub name: String,
    pub description: String,
    pub members: Vec<TeamMember>,
    pub shared_context: HashMap<String, String>,
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
#[derive(Clone, Debug, Default)]
pub(super) struct TeamRegistry {
    teams: HashMap<String, Team>,
    history: Vec<TeamHistoryEntry>,
    snapshots: Vec<TeamSnapshotEntry>,
}

impl TeamRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            teams: HashMap::new(),
            history: Vec::new(),
            snapshots: Vec::new(),
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
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        );

        // Research team: explorer + synthesizer
        self.teams.insert(
            "research".to_string(),
            Team {
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
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        );

        // Full dev team: planner + implementer + tester
        self.teams.insert(
            "dev".to_string(),
            Team {
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
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<&Team> {
        self.teams.get(name)
    }

    pub fn list(&self) -> Vec<&Team> {
        let mut teams: Vec<_> = self.teams.values().collect();
        teams.sort_by_key(|t| &t.name);
        teams
    }

    pub fn create(&mut self, name: String, description: String) -> Result<(), String> {
        if self.teams.contains_key(&name) {
            return Err(format!("Team '{name}' already exists"));
        }
        self.teams.insert(
            name.clone(),
            Team {
                name,
                description,
                members: Vec::new(),
                shared_context: HashMap::new(),
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

    let coordination = infer_coordination(team);
    let now = chrono::Utc::now().to_rfc3339();
    TeamDefinition {
        team_id: uuid::Uuid::new_v4().to_string(),
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
        worktree_mode: WorktreeMode::Shared,
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

pub(super) async fn handle_team_command(arg: &str, state: &mut super::ReplState) {
    let mut parts = arg.splitn(2, ' ');
    let sub = parts.next().unwrap_or("").trim();
    let sub_arg = parts.next().unwrap_or("").trim();

    match sub {
        "" | "help" => {
            eprintln!(
                "\n{}",
                "─── Team ───────────────────────────────────────".bold()
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
                "─── Teams ───────────────────────────────────────────────".bold()
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
            let mut parts = sub_arg.splitn(2, ' ');
            let name = parts.next().unwrap_or("").trim();
            let desc = parts.next().unwrap_or("").trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /team create <name> [description]".yellow());
                return;
            }
            let description = if desc.is_empty() {
                format!("Custom team: {name}")
            } else {
                desc.to_string()
            };
            match state.team_registry.create(name.to_string(), description) {
                Ok(()) => {
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
            // /team run <team> <task description>
            let mut parts = sub_arg.splitn(2, ' ');
            let team_name = parts.next().unwrap_or("").trim();
            let task = parts.next().unwrap_or("").trim();
            if team_name.is_empty() || task.is_empty() {
                eprintln!(
                    "{}",
                    "  Usage: /team run <team> <task description>".yellow()
                );
                eprintln!(
                    "{}",
                    "  Executes a team task through the delegation engine.".dim()
                );
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

            let delegation_engine = match state.delegation_engine {
                Some(ref e) => e.clone(),
                None => {
                    eprintln!(
                        "  {} Delegation engine not available (not logged in?)",
                        theme::icon_err()
                    );
                    return;
                }
            };

            let user_id = state
                .ingestion_user_id
                .clone()
                .unwrap_or_else(|| "local".into());
            let session_id = state
                .session_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            // Convert CLI team → TeamDefinition for the orchestrator
            let team_def = cli_team_to_definition(&cli_team, &user_id);

            // Build team store with this team pre-loaded
            // Use the shared team store; ensure this team is loaded into it
            let team_store = state.team_store.clone();
            if let Err(e) = team_store.save_team(&team_def).await {
                eprintln!("  {} Failed to prepare team store: {e}", theme::icon_err());
                return;
            }

            // Reuse the delegation engine's shared registry and run engine
            let profile_registry = delegation_engine.registry().clone();
            let run_engine = delegation_engine.run_engine().clone();

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
                });

            let config = astra_runtime::server::team_orchestrator::OrchestratorConfig {
                user_id: user_id.clone(),
                session_id,
                source_agent_id: "orchestrator".to_string(),
                progress: Some(progress),
            };

            let orchestrator =
                astra_runtime::server::team_orchestrator::TeamExecutionOrchestrator::new(
                    team_store,
                    delegation_engine.clone(),
                    delegation_engine.tracker().clone(),
                    run_engine,
                    profile_registry,
                    config,
                );

            // Print header
            eprintln!("\n{}", format!("─── Team Run: {} ───", team_name).bold());
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

            // Record execution start in persistence store (best-effort)
            let exec_id = uuid::Uuid::new_v4().to_string();
            let _ = state
                .team_store
                .record_execution_start(
                    &exec_id, team_name, "", // user_id (empty for CLI sessions)
                    task,
                )
                .await;

            let repo_root = std::env::current_dir().ok();
            let report = orchestrator.execute_team(team_name, task, repo_root).await;
            let elapsed = timer.elapsed();

            // Display report header with status
            eprintln!();
            match report.status {
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::Completed => {
                    eprintln!(
                        "  {} Team '{}' completed successfully {}",
                        "✅", team_name.green().bold(),
                        format!("({})", format_duration(elapsed)).dim()
                    );
                }
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::CompletedWithConflicts => {
                    eprintln!(
                        "  {} Team '{}' completed with merge conflicts {}",
                        "⚠️ ", team_name.yellow().bold(),
                        format!("({})", format_duration(elapsed)).dim()
                    );
                }
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::Failed => {
                    eprintln!(
                        "  {} Team '{}' execution failed {}",
                        theme::icon_err(), team_name.red().bold(),
                        format!("({})", format_duration(elapsed)).dim()
                    );
                    if let Some(ref err) = report.error {
                        eprintln!("    {} {}", "Error:".red().bold(), err);
                    }
                }
            }

            // Agent results
            if let Some(ref dr) = report.delegation_result {
                let total_tokens = dr.total_prompt_tokens + dr.total_completion_tokens;
                eprintln!(
                    "\n  {} {} agent{} | {} tokens ({}↑ {}↓) | delegation {}",
                    "📊",
                    dr.agent_results.len(),
                    if dr.agent_results.len() == 1 { "" } else { "s" },
                    format_tokens(total_tokens),
                    format_tokens(dr.total_prompt_tokens),
                    format_tokens(dr.total_completion_tokens),
                    dr.delegation_id.get(..8).unwrap_or(&dr.delegation_id).dim(),
                );
                for ar in &dr.agent_results {
                    let (status_icon, status_color) = if ar.status == "completed" {
                        ("✓", "green")
                    } else {
                        ("✗", "red")
                    };
                    let first_line = ar
                        .output
                        .as_deref()
                        .unwrap_or("(no output)")
                        .lines()
                        .next()
                        .unwrap_or("");
                    let agent_tokens = ar.prompt_tokens + ar.completion_tokens;
                    match status_color {
                        "green" => eprintln!(
                            "    {} {} {} — {}",
                            status_icon.green(),
                            ar.agent_id.as_str().cyan(),
                            format!("({}tok)", format_tokens(agent_tokens)).dim(),
                            truncate_str(first_line, 72),
                        ),
                        _ => eprintln!(
                            "    {} {} {} — {}",
                            status_icon.red(),
                            ar.agent_id.as_str().cyan(),
                            format!("({}tok)", format_tokens(agent_tokens)).dim(),
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
                            "\n  {} Merge: {} branch{} merged cleanly",
                            "🔀",
                            merge.merged.len(),
                            if merge.merged.len() == 1 { "" } else { "es" }
                        );
                    }
                } else {
                    eprintln!(
                        "\n  {} Merge: {} conflict{}",
                        "🔀",
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
                        "\n  {} Learning from {} agent{}:",
                        "🧠",
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

            // Persist execution complete to the store (best-effort)
            let result_summary = serde_json::json!({
                "agent_count": agent_count,
                "prompt_tokens": prompt_tok,
                "completion_tokens": compl_tok,
                "delegation_id": &report.delegation_id,
                "error": &report.error,
            });
            let _ = state
                .team_store
                .record_execution_complete(
                    &report.delegation_id,
                    &report.status.to_string(),
                    Some(&result_summary.to_string()),
                )
                .await;
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
            if state.team_registry.get(name).is_none() {
                eprintln!("  {} Team '{}' not found", theme::icon_err(), name);
                return;
            }

            // Check persistence store for additional records
            let store_entries = state
                .team_store
                .list_executions(name, 50)
                .await
                .unwrap_or_default();
            let registry_entries = state.team_registry.get_history(name);

            if registry_entries.is_empty() && store_entries.is_empty() {
                eprintln!(
                    "\n  {} No execution history for team '{}'.",
                    "📜",
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
                    eprintln!("    {} {}", "Error:".red(), truncate_str(err, 60));
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

            eprintln!(
                "\n  {} Restoring team '{}' from snapshot '{}'...",
                "⏪",
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
    "Subcommands: /team list · info · create · add-member · context · run · history · snapshot · restore · delete · help"
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
        reg.create("my-team".into(), "test team".into()).unwrap();
        assert!(reg.get("my-team").is_some());
        assert_eq!(reg.list().len(), 4);

        reg.remove("my-team").unwrap();
        assert!(reg.get("my-team").is_none());
    }

    #[test]
    fn create_duplicate_fails() {
        let mut reg = TeamRegistry::new();
        assert!(reg.create("review".into(), "dup".into()).is_err());
    }

    #[test]
    fn add_member_to_team() {
        let mut reg = TeamRegistry::new();
        reg.create("test".into(), "test".into()).unwrap();
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
        reg.create("test".into(), "test".into()).unwrap();
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
}
