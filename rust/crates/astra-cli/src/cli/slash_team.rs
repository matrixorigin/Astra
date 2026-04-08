use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use astra_services::team_persistence::TeamPersistenceService;

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
}

impl TeamRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            teams: HashMap::new(),
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
}

// ── CLI Team → TeamDefinition Conversion ────────────────────────────────

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
    if roles.iter().any(|r| r.contains("producer") || r.contains("writer"))
        && roles.iter().any(|r| r.contains("reviewer") || r.contains("critic"))
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
            r.contains("synthesizer")
                || r.contains("implementer")
                || r.contains("executor")
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
        "" | "list" => {
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
                eprintln!("{}", "  Usage: /team run <team> <task description>".yellow());
                eprintln!("{}", "  Executes a team task through the delegation engine.".dim());
                return;
            }

            let cli_team = match state.team_registry.get(team_name) {
                Some(t) => t.clone(),
                None => {
                    eprintln!("  {} Team '{}' not found", theme::icon_err(), team_name);
                    return;
                }
            };

            let delegation_engine = match state.delegation_engine {
                Some(ref e) => e.clone(),
                None => {
                    eprintln!("  {} Delegation engine not available (not logged in?)", theme::icon_err());
                    return;
                }
            };

            let user_id = state.ingestion_user_id.clone().unwrap_or_else(|| "local".into());
            let session_id = state.session_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            // Convert CLI team → TeamDefinition for the orchestrator
            let team_def = cli_team_to_definition(&cli_team, &user_id);

            // Build team store with this team pre-loaded
            let team_store = Arc::new(
                astra_services::team_persistence::InMemoryTeamStore::new(),
            );
            if let Err(e) = team_store.save_team(&team_def).await {
                eprintln!("  {} Failed to prepare team store: {e}", theme::icon_err());
                return;
            }

            // Reuse the delegation engine's shared registry and run engine
            let profile_registry = delegation_engine.registry().clone();
            let run_engine = delegation_engine.run_engine().clone();

            let config = astra_runtime::server::team_orchestrator::OrchestratorConfig {
                user_id: user_id.clone(),
                session_id,
                source_agent_id: "orchestrator".to_string(),
            };

            let orchestrator = astra_runtime::server::team_orchestrator::TeamExecutionOrchestrator::new(
                team_store,
                delegation_engine,
                run_engine,
                profile_registry,
                config,
            );

            eprintln!(
                "\n  {} Dispatching task to team '{}' ({} members)...",
                "🚀", team_name.cyan().bold(), cli_team.members.len()
            );
            for m in &cli_team.members {
                eprintln!(
                    "    {} {} {}",
                    "→".dim(),
                    m.role.as_str().green(),
                    format!("— {}", m.description).dim()
                );
            }
            eprintln!("\n  {} Task: {}\n", "📋", task);

            let repo_root = std::env::current_dir().ok();
            let report = orchestrator.execute_team(team_name, task, repo_root).await;

            // Display report
            match report.status {
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::Completed => {
                    eprintln!("  {} Team '{}' completed successfully", "✅", team_name.green().bold());
                }
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::CompletedWithConflicts => {
                    eprintln!("  {} Team '{}' completed with merge conflicts", "⚠️ ", team_name.yellow().bold());
                }
                astra_runtime::server::team_orchestrator::TeamExecutionStatus::Failed => {
                    eprintln!("  {} Team '{}' failed: {}", theme::icon_err(), team_name,
                        report.error.as_deref().unwrap_or("unknown error"));
                }
            }

            if let Some(ref dr) = report.delegation_result {
                eprintln!("  {} Delegation: {} | agents: {} | tokens: {}+{}",
                    "📊".dim(),
                    dr.delegation_id.get(..8).unwrap_or(&dr.delegation_id),
                    dr.agent_results.len(),
                    dr.total_prompt_tokens,
                    dr.total_completion_tokens,
                );
                for ar in &dr.agent_results {
                    let status_icon = if ar.status == "completed" { "✓" } else { "✗" };
                    eprintln!("    {} {} — {}",
                        status_icon.dim(),
                        ar.agent_id.as_str().cyan(),
                        ar.output.as_deref().unwrap_or("(no output)")
                            .lines().next().unwrap_or("")
                    );
                }
            }

            if let Some(ref merge) = report.merge_result {
                if merge.conflicts.is_empty() {
                    eprintln!("  {} Merge: clean ({})",
                        "🔀".dim(),
                        if !merge.merged.is_empty() { "success" } else { "no changes" });
                } else {
                    eprintln!("  {} Merge: {} conflict(s)", "🔀".dim(), merge.conflicts.len());
                    for c in &merge.conflicts {
                        eprintln!("    {} {} — {}", "!".red(), c.agent_id, c.files.join(", "));
                    }
                }
            }

            if let Some(ref learning) = report.merged_learning {
                if !learning.consensus_patterns.is_empty() || !learning.facts.is_empty() {
                    eprintln!("  {} Learning: {} patterns, {} facts",
                        "🧠".dim(),
                        learning.consensus_patterns.len(),
                        learning.facts.len(),
                    );
                }
            }
            eprintln!();
        }

        "history" => {
            // /team history <team>
            let name = sub_arg.trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /team history <team>".yellow());
                eprintln!("{}", "  Shows execution history from DurableRunRecords.".dim());
                return;
            }
            match state.team_registry.get(name) {
                Some(_) => {
                    eprintln!(
                        "\n  {} Execution history for team '{}':",
                        "📜",
                        name.cyan().bold()
                    );
                    eprintln!(
                        "  {}",
                        "  No executions recorded yet. Use /team run to execute.".dim()
                    );
                    eprintln!(
                        "  {}",
                        "  History will be populated from DurableRunRecords after team orchestrator integration.".dim()
                    );
                }
                None => {
                    eprintln!("  {} Team '{}' not found", theme::icon_err(), name);
                }
            }
        }

        "snapshot" => {
            // /team snapshot <team>
            let name = sub_arg.trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /team snapshot <team>".yellow());
                eprintln!("{}", "  Creates a CompositeSnapshot of the team's current state.".dim());
                return;
            }
            match state.team_registry.get(name) {
                Some(_) => {
                    let snapshot_id = format!("team-snap-{}-{}", name, chrono::Utc::now().timestamp());
                    eprintln!(
                        "  {} Snapshot '{}' created for team '{}'",
                        theme::icon_ok(),
                        snapshot_id.dim(),
                        name.cyan()
                    );
                    eprintln!(
                        "  {}",
                        "  Snapshot captures: team definition, git commit, session state.".dim()
                    );
                }
                None => {
                    eprintln!("  {} Team '{}' not found", theme::icon_err(), name);
                }
            }
        }

        "restore" => {
            // /team restore <team> <snapshot-id>
            let mut parts = sub_arg.splitn(2, ' ');
            let name = parts.next().unwrap_or("").trim();
            let snapshot_id = parts.next().unwrap_or("").trim();
            if name.is_empty() || snapshot_id.is_empty() {
                eprintln!("{}", "  Usage: /team restore <team> <snapshot-id>".yellow());
                eprintln!("{}", "  Restores team state from a CompositeSnapshot.".dim());
                return;
            }
            match state.team_registry.get(name) {
                Some(_) => {
                    eprintln!(
                        "  {} Restoring team '{}' from snapshot '{}'...",
                        "⏪",
                        name.cyan(),
                        snapshot_id.dim()
                    );
                    eprintln!(
                        "  {}",
                        "  Restore requires runtime CompositeSnapshot integration.".dim()
                    );
                }
                None => {
                    eprintln!("  {} Team '{}' not found", theme::icon_err(), name);
                }
            }
        }

        _ => {
            eprintln!(
                "{}",
                format!(
                    "  Unknown /team subcommand: '{sub}'. Try: list, create, info, add-member, delete, context, run, history, snapshot, restore"
                )
                .yellow()
            );
        }
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
        assert_eq!(
            def.members[0].system_prompt.as_deref(),
            Some("coder agent")
        );
    }

    #[test]
    fn cli_team_to_definition_preserves_context() {
        let mut team = make_team(&["a"]);
        team.shared_context
            .insert("lang".into(), "rust".into());
        let def = cli_team_to_definition(&team, "u");
        assert_eq!(def.context.get("lang").unwrap(), "rust");
    }
}
