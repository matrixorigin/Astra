use super::*;
use std::collections::HashMap;

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

// ── Slash Command Handler ───────────────────────────────────────────────

pub(super) fn handle_team_command(arg: &str, state: &mut super::ReplState) {
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
                            let preview = if v.len() > 60 {
                                format!("{}...", &v[..60])
                            } else {
                                v.clone()
                            };
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
                        if value.len() > 40 {
                            format!("{}...", &value[..40])
                        } else {
                            value.to_string()
                        },
                        team.cyan()
                    );
                }
                Err(e) => eprintln!("  {} {e}", theme::icon_err()),
            }
        }

        _ => {
            eprintln!(
                "{}",
                format!(
                    "  Unknown /team subcommand: '{sub}'. Try: list, create, info, add-member, delete, context"
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
}
