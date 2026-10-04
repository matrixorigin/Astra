use crate::cli::{
    cli_config::cli_utils::truncate_str, session::session_state::SessionState, theme,
};
use astra_services::team_persistence::TeamPersistenceService;
use crossterm::style::Stylize;
use std::collections::HashMap;

/// Typed input for the existing ordinary Chat command. This is only a
/// configuration carrier; admission, execution, cancellation, and recovery
/// remain owned by the normal root-turn path.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TeamChatRequest {
    pub message: String,
    pub selection: astra_services::runs::AgentProfileSelection,
}

pub(crate) async fn resolve_team_run_chat_request(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    team_name_or_id: &str,
    lead_agent_id: Option<&str>,
    task: &str,
) -> Result<TeamChatRequest, String> {
    if task.trim().is_empty() {
        return Err("Team run task cannot be empty".into());
    }
    let store = crate::cli::http_team_store::HttpTeamStore::new(api, profile);
    let team = TeamPersistenceService::load_team(&store, "", team_name_or_id)
        .await
        .map_err(|error| format!("failed to load team '{team_name_or_id}': {error}"))?
        .ok_or_else(|| format!("Team '{team_name_or_id}' not found"))?;
    if team.members.is_empty() {
        return Err(format!(
            "Team '{}' has no members. Add a member before starting a lead turn.",
            team.name
        ));
    }
    let mut profiles = team
        .members
        .iter()
        .map(|member| astra_services::team_persistence::resolve_member_to_profile(member, &team));
    let lead = match lead_agent_id {
        Some(id) => profiles
            .find(|profile| profile.agent_id == id.trim())
            .ok_or_else(|| {
                format!(
                    "lead '{}' is not a member of Team '{}'; use an Agent ID shown by `team info`",
                    id, team.name
                )
            })?,
        None => {
            let mut coordinators = profiles.filter(|profile| profile.can_delegate);
            let lead = coordinators.next().ok_or_else(|| {
                format!("Team '{}' has no delegation-capable coordinator; select a member with --lead-agent-id", team.name)
            })?;
            if coordinators.next().is_some() {
                return Err(format!(
                    "Team '{}' has multiple delegation-capable coordinators; select one with --lead-agent-id",
                    team.name
                ));
            }
            lead
        }
    };
    Ok(TeamChatRequest {
        message: task.trim().to_string(),
        selection: astra_services::runs::AgentProfileSelection {
            team_id: team.team_id,
            lead_agent_id: Some(lead.agent_id),
        },
    })
}

// ── Team Registry ───────────────────────────────────────────────────────

/// The CLI registry stores the persistence owner's canonical definition
/// directly. There is no second lossy Team/TeamMember schema in the CLI.
pub(crate) type Team = astra_services::team_persistence::TeamDefinition;
pub(crate) type TeamMember = astra_services::team_persistence::TeamMemberDef;

/// Registry of all defined teams (stored in SessionState).
#[derive(Clone, Debug)]
pub(crate) struct TeamRegistry {
    teams: HashMap<String, Team>,
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
        Self {
            teams: HashMap::new(),
            store_loaded: false,
        }
    }

    /// Merge the owner-scoped persistence projection into the registry.
    ///
    /// Persistence is authoritative: a remote definition with the same name
    /// replaces any stale in-process projection, including a definition that
    /// was present before hydration.
    pub fn merge_from_store(
        &mut self,
        teams: Vec<astra_services::team_persistence::TeamDefinition>,
    ) {
        for def in teams {
            self.teams.insert(def.name.clone(), def);
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

    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        if self.teams.remove(name).is_none() {
            return Err(format!("Team '{name}' not found"));
        }
        Ok(())
    }
}

/// Get current git HEAD commit SHA (best-effort).
fn git_head_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn team_member_description(member: &TeamMember) -> String {
    member
        .system_prompt
        .clone()
        .unwrap_or_else(|| format!("{} agent", member.role))
}

// ── Slash Command Handler ───────────────────────────────────────────────

pub(crate) async fn handle_team_command(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
) -> Result<(), String> {
    // Hydrate registry from persistence store on first command
    if !state.team_registry.store_loaded {
        let user_id = state
            .ingestion_user_id
            .clone()
            .unwrap_or_else(|| "local".into());
        let teams = state
            .team_store
            .list_teams(&user_id)
            .await
            .map_err(|error| format!("failed to hydrate teams: {error}"))?;
        state.team_registry.merge_from_store(teams);
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
                    .magenta()
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
                    names.magenta().to_string()
                }
            );
            eprintln!(
                "  {:<16} {}",
                "built-ins:".dim(),
                "owner-scoped team service".magenta()
            );
            eprintln!();
            eprintln!("  {}", team_subcommands_hint().dim());
            eprintln!("  {}", "Examples:".dim());
            eprintln!("    {}", "/team info review".magenta());
            eprintln!(
                "    {}",
                "/team run review --lead-agent-id team-review-reviewer review the latest diff"
                    .magenta()
            );
            eprintln!("    {}", "/team snapshot dev before-refactor".magenta());
            eprintln!();
        }

        "list" => {
            let teams = state.team_registry.list();
            if teams.is_empty() {
                eprintln!(
                    "  {}",
                    "No teams defined. Use /team create <name> <description>".dim()
                );
                return Ok(());
            }
            eprintln!(
                "\n{}",
                "─── Teams ───────────────────────────────────────────────"
                    .bold()
                    .magenta()
            );
            for t in &teams {
                eprintln!(
                    "\n  {} {}",
                    t.name.as_str().magenta().bold(),
                    format!("({})", t.description).dim()
                );
                if t.members.is_empty() {
                    eprintln!("    {}", "No members. Use /team add-member".dim());
                } else {
                    for m in &t.members {
                        let agent_id =
                            astra_services::team_persistence::resolve_member_to_profile(m, t)
                                .agent_id;
                        eprintln!(
                            "    {} {} [{}] {}",
                            "•".dim(),
                            m.role.as_str().green(),
                            agent_id.dim(),
                            format!("— {}", team_member_description(m)).dim()
                        );
                    }
                }
                if !t.context.is_empty() {
                    eprintln!(
                        "    {} shared keys: {}",
                        "📎".to_string().dim(),
                        t.context
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
            // /team create <name> [description]
            let mut parts = sub_arg.splitn(2, ' ');
            let name = parts.next().unwrap_or("").trim();
            let rest = parts.next().unwrap_or("").trim();
            if name.is_empty() {
                return Err("Usage: /team create <name> [description]".into());
            }
            let description = if rest.is_empty() {
                format!("Custom team: {name}")
            } else {
                rest.to_string()
            };
            let user_id = state
                .ingestion_user_id
                .clone()
                .unwrap_or_else(|| "local".into());
            if state.team_registry.get(name).is_some() {
                return Err(format!("Team '{name}' already exists"));
            }
            let now = chrono::Utc::now().to_rfc3339();
            let definition = Team {
                team_id: uuid::Uuid::new_v4().to_string(),
                user_id,
                name: name.to_string(),
                description,
                members: Vec::new(),
                context: HashMap::new(),
                created_at: now.clone(),
                updated_at: now,
            };
            let persisted = state
                .team_store
                .save_team(&definition)
                .await
                .map_err(|error| format!("failed to persist team '{name}': {error}"))?;
            state.team_registry.merge_from_store(vec![persisted]);
            eprintln!(
                "  {} Team '{}' created. Add members with /team add-member {} <role> <description>",
                theme::icon_ok(),
                name.magenta(),
                name
            );
        }

        "add-member" => {
            let crate::cli::cli_config::cli_args::Command::Team(args) =
                crate::cli::command_router::parse_team_bridge_command(arg)?
            else {
                return Err("expected a Team command".into());
            };
            let Some(crate::cli::cli_config::cli_args::TeamSubcommand::AddMember(member_args)) =
                args.command
            else {
                return Err("expected add-member arguments".into());
            };
            let team = member_args.team.as_str();
            let role = member_args.role.as_str();
            let desc = member_args.description.join(" ");
            let model_selection = if let Some(model) = member_args.model.as_deref() {
                let token = crate::cli::session::session_runtime::fresh_access_token(api, profile)
                    .await
                    .ok_or_else(|| "Not logged in".to_string())?;
                let selected =
                    crate::cli::session::session_runtime::resolve_server_model_selection(
                        api,
                        &token,
                        model,
                        astra_core::model_wire::purpose::ModelCatalogPurpose::Chat,
                    )
                    .await?;
                Some(astra_turn_types::ModelSelection {
                    offering_id: selected.offering_id,
                })
            } else {
                None
            };
            let member = TeamMember {
                role: role.to_string(),
                agent_id: None,
                system_prompt: Some(if desc.is_empty() {
                    format!("{role} agent")
                } else {
                    desc
                }),
                skills: Vec::new(),
                model_selection,
                mcp_servers: Vec::new(),
                can_delegate: member_args.can_delegate,
                max_delegation_depth: member_args.max_delegation_depth.unwrap_or(0),
                ..Default::default()
            };
            let mut definition = state
                .team_registry
                .get(team)
                .cloned()
                .ok_or_else(|| format!("Team '{team}' not found"))?;
            if definition
                .members
                .iter()
                .any(|existing| existing.role == role)
            {
                return Err(format!("Role '{}' already exists in team '{team}'", role));
            }
            definition.members.push(member);
            let persisted = state
                .team_store
                .save_team(&definition)
                .await
                .map_err(|error| format!("failed to persist team '{team}': {error}"))?;
            state.team_registry.merge_from_store(vec![persisted]);
            eprintln!(
                "  {} Added role '{}' to team '{}'",
                theme::icon_ok(),
                role.green(),
                team.magenta()
            );
        }

        "info" => {
            let name = sub_arg.trim();
            if name.is_empty() {
                return Err("Usage: /team info <name>".into());
            }
            match state.team_registry.get(name) {
                Some(t) => {
                    eprintln!(
                        "\n  {} {}",
                        "Team:".bold(),
                        t.name.as_str().magenta().bold()
                    );
                    eprintln!("  {} {}", "Description:".dim(), t.description);
                    eprintln!("  {} {}", "Created:".dim(), t.created_at);
                    eprintln!("\n  {}", "Members:".bold());
                    for m in &t.members {
                        let agent_id =
                            astra_services::team_persistence::resolve_member_to_profile(m, t)
                                .agent_id;
                        eprintln!(
                            "    {} {} [{}] — {}",
                            "•".dim(),
                            m.role.as_str().green(),
                            agent_id.dim(),
                            team_member_description(m)
                        );
                        if !m.skills.is_empty() {
                            eprintln!("      {} {}", "Skills:".dim(), m.skills.join(", "));
                        }
                        if let Some(ref model) = m.model_selection {
                            eprintln!("      {} {}", "Offering:".dim(), model.offering_id);
                        }
                    }
                    if !t.context.is_empty() {
                        eprintln!("\n  {}", "Shared Context:".bold());
                        for (k, v) in &t.context {
                            let preview = truncate_str(v, 60);
                            eprintln!("    {} = {}", k.as_str().magenta(), preview);
                        }
                    }
                    eprintln!();
                }
                None => {
                    return Err(format!("Team '{name}' not found"));
                }
            }
        }

        "delete" => {
            let name = sub_arg.trim();
            if name.is_empty() {
                return Err("Usage: /team delete <name>".into());
            }
            if state.team_registry.get(name).is_none() {
                return Err(format!("Team '{name}' not found"));
            }
            let user_id = state
                .ingestion_user_id
                .clone()
                .unwrap_or_else(|| "local".into());
            let deleted = state
                .team_store
                .delete_team(&user_id, name)
                .await
                .map_err(|error| format!("failed to delete team '{name}': {error}"))?;
            if !deleted {
                return Err(format!("Team '{name}' was not found in persistence store"));
            }
            state.team_registry.remove(name)?;
            eprintln!("  {} Team '{}' deleted", theme::icon_ok(), name);
        }

        "context" => {
            // /team context <team> <key> <value>
            let mut parts = sub_arg.splitn(3, ' ');
            let team = parts.next().unwrap_or("").trim();
            let key = parts.next().unwrap_or("").trim();
            let value = parts.next().unwrap_or("").trim();
            if team.is_empty() || key.is_empty() {
                return Err("Usage: /team context <team> <key> <value>".into());
            }
            let mut candidate = state
                .team_registry
                .get(team)
                .cloned()
                .ok_or_else(|| format!("Team '{team}' not found"))?;
            candidate.context.insert(key.to_string(), value.to_string());
            let persisted = state
                .team_store
                .save_team(&candidate)
                .await
                .map_err(|error| format!("failed to persist team '{team}': {error}"))?;
            state.team_registry.merge_from_store(vec![persisted]);
            eprintln!(
                "  {} Set context '{}'='{}' on team '{}'",
                theme::icon_ok(),
                key,
                truncate_str(value, 40),
                team.magenta()
            );
        }

        "snapshot" => {
            // /team snapshot <team> [label]
            let mut parts = sub_arg.splitn(2, ' ');
            let name = parts.next().unwrap_or("").trim();
            let label = parts.next().unwrap_or("").trim();
            if name.is_empty() {
                return Err("Usage: /team snapshot <team> [label]".into());
            }
            let team_definition = state
                .team_registry
                .get(name)
                .cloned()
                .ok_or_else(|| format!("Team '{name}' not found"))?;

            let snapshot_id = format!("team-{}-{}", name, chrono::Utc::now().timestamp());
            let git_sha = git_head_sha();
            let session_id = state.session_id.clone();
            let now = chrono::Utc::now().to_rfc3339();

            let snap_label = if label.is_empty() {
                format!("team {} snapshot", name)
            } else {
                label.to_string()
            };

            // Persist the complete canonical definition, not a display-only
            // projection that would discard member capabilities or context.
            let team_def_json = Some(
                serde_json::to_string(&team_definition)
                    .map_err(|error| format!("failed to encode team snapshot: {error}"))?,
            );
            let snap_record = astra_services::team_persistence::TeamSnapshotRecord {
                snapshot_id: snapshot_id.clone(),
                team_name: name.to_string(),
                user_id: team_definition.user_id.clone(),
                label: snap_label.clone(),
                git_commit: git_sha.clone(),
                session_id: session_id.clone(),
                team_definition_json: team_def_json,
                created_at: now.clone(),
            };
            let accepted = state
                .team_store
                .save_snapshot(&snap_record)
                .await
                .map_err(|error| format!("failed to persist snapshot: {error}"))?;
            eprintln!(
                "\n  {} Snapshot '{}' created for team '{}'",
                theme::icon_ok(),
                accepted.snapshot_id.as_str().dim(),
                name.magenta()
            );
            if let Some(ref sha) = accepted.git_commit {
                eprintln!("    {} Git: {}", "🔖".dim(), sha.get(..12).unwrap_or(sha),);
            }
            eprintln!(
                "    {} Use '/team restore {} {}' to restore.",
                "💡".dim(),
                name,
                accepted.snapshot_id,
            );
            eprintln!();
        }

        "restore" => {
            let mut parts = sub_arg.splitn(2, ' ');
            let name = parts.next().unwrap_or("").trim();
            let snapshot_id = parts.next().unwrap_or("").trim();
            if name.is_empty() || snapshot_id.is_empty() {
                return Err("Usage: /team restore <team> <snapshot-id>".into());
            }
            let current = state
                .team_registry
                .get(name)
                .cloned()
                .ok_or_else(|| format!("Team '{name}' not found"))?;
            let snap = state
                .team_store
                .find_snapshot(snapshot_id, &current.user_id)
                .await
                .map_err(|error| format!("failed to load snapshot '{snapshot_id}': {error}"))?
                .ok_or_else(|| format!("Snapshot '{snapshot_id}' not found"))?;
            if snap.team_name != name {
                return Err(format!(
                    "Snapshot '{}' belongs to team '{}', not '{}'",
                    snap.snapshot_id, snap.team_name, name
                ));
            }
            let definition_json = snap
                .team_definition_json
                .as_deref()
                .ok_or_else(|| "Snapshot has no Team configuration".to_string())?;
            let mut definition: Team = serde_json::from_str(definition_json)
                .map_err(|error| format!("invalid snapshot configuration: {error}"))?;
            if definition.name != current.name || definition.user_id != current.user_id {
                return Err("Snapshot configuration belongs to another Team or owner".into());
            }
            astra_services::team_persistence::validate_team(&definition)
                .map_err(|errors| format!("invalid snapshot configuration: {errors:?}"))?;
            // Restore configuration, never historical identity or Git state.
            definition.team_id = current.team_id;
            definition.created_at = current.created_at;
            definition.updated_at = chrono::Utc::now().to_rfc3339();
            let accepted = state
                .team_store
                .save_team(&definition)
                .await
                .map_err(|error| format!("failed to restore Team configuration: {error}"))?;
            state.team_registry.merge_from_store(vec![accepted]);
            eprintln!(
                "  {} Team configuration restored; Git and running tasks unchanged.\n",
                theme::icon_ok()
            );
        }

        _ => return Err(format!("Unknown /team subcommand: '{sub}'")),
    }
    Ok(())
}

fn team_subcommands_hint() -> &'static str {
    "Subcommands: /team list · info · create · add-member · context · run · snapshot · restore · delete · help"
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{Team, TeamMember, TeamRegistry, git_head_sha, team_subcommands_hint};
    use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
    use crate::cli::session::session_state::SessionState;
    use std::collections::HashMap;

    #[test]
    fn registry_starts_empty_until_remote_hydration() {
        let reg = TeamRegistry::new();
        assert!(reg.list().is_empty());
        assert!(!reg.store_loaded);
    }

    #[test]
    fn remote_projection_can_be_published_and_deleted() {
        let mut reg = TeamRegistry::new();
        reg.merge_from_store(vec![make_team(&["coder"])]);
        assert!(reg.get("test").is_some());
        assert_eq!(reg.list().len(), 1);

        reg.remove("test").unwrap();
        assert!(reg.get("test").is_none());
    }

    #[test]
    fn canonical_projection_preserves_member_and_team_fields() {
        let mut team = make_team(&["coder"]);
        team.members[0].agent_id = Some("stable-coder".into());
        team.members[0].mcp_servers = vec!["docs".into()];
        team.members[0].can_delegate = true;
        team.members[0].max_delegation_depth = 2;

        let mut reg = TeamRegistry::new();
        reg.merge_from_store(vec![team.clone()]);
        let stored = reg.get("test").unwrap();
        assert_eq!(stored.team_id, team.team_id);
        assert_eq!(stored.members[0].agent_id, team.members[0].agent_id);
        assert_eq!(stored.members[0].mcp_servers, team.members[0].mcp_servers);
        assert_eq!(stored.members[0].can_delegate, team.members[0].can_delegate);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn failed_team_http_commands_leave_the_published_projection_unchanged() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                access_token: Some("test-token".into()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/teams"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/teams"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let original = make_team(&["first"]);
        let expected = serde_json::to_value(&original).unwrap();
        let mut state = SessionState::default();
        state.team_store =
            std::sync::Arc::new(crate::cli::http_team_store::HttpTeamStore::new(&api, None));
        state.team_registry.merge_from_store(vec![original]);
        state.team_registry.store_loaded = true;
        for command in [
            "create fresh",
            "add-member test second",
            "context test key value",
        ] {
            assert!(
                super::handle_team_command(command, &api, None, &mut state)
                    .await
                    .is_err()
            );
            assert_eq!(
                serde_json::to_value(state.team_registry.get("test").unwrap()).unwrap(),
                expected
            );
            assert!(state.team_registry.get("fresh").is_none());
        }
        state.team_registry.store_loaded = false;
        assert!(
            super::handle_team_command("list", &api, None, &mut state)
                .await
                .is_err()
        );
        assert!(!state.team_registry.store_loaded);
        assert_eq!(
            serde_json::to_value(state.team_registry.get("test").unwrap()).unwrap(),
            expected
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[test]
    fn remove_nonexistent_fails() {
        let mut reg = TeamRegistry::new();
        assert!(reg.remove("ghost").is_err());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn snapshot_and_cold_restore_preserve_owner_and_configuration() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                access_token: Some("test-token".into()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();
        let server = wiremock::MockServer::start().await;
        let mut saved = make_team(&["first"]);
        saved.context.insert("contract".into(), "before".into());
        saved.members[0].mcp_servers = vec!["fixture-mcp".into()];
        saved.members[0].can_delegate = true;
        saved.members[0].max_delegation_depth = 2;
        let snapshot = serde_json::json!({
            "snapshot_id": "snap-server-identity", "team_name": "test",
            "label": "accepted-label", "git_commit": "not-a-checkout-target",
            "session_id": null, "team_definition_json": serde_json::to_string(&saved).unwrap(),
            "created_at": "2026-10-03T00:00:00Z",
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/teams"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"teams": [&saved]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/teams/test/snapshots"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&snapshot))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let store =
            std::sync::Arc::new(crate::cli::http_team_store::HttpTeamStore::new(&api, None));
        let mut initial = SessionState::default();
        initial.team_store = store.clone();
        super::handle_team_command("snapshot test local-label", &api, None, &mut initial)
            .await
            .unwrap();
        assert_eq!(initial.team_registry.get("test").unwrap().user_id, "u");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        server.reset().await;

        let mut current = make_team(&["second"]);
        current.context.insert("contract".into(), "after".into());
        let mut expected = saved.clone();
        expected.team_id = current.team_id.clone();
        expected.created_at = current.created_at.clone();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/teams"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"teams": [&current]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/teams/snapshots/snap-server-identity",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&snapshot))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/teams"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "context": saved.context, "members": saved.members,
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&expected))
            .expect(1)
            .mount(&server)
            .await;
        let mut state = SessionState::default();
        state.team_store = store;
        super::handle_team_command("restore test snap-server-identity", &api, None, &mut state)
            .await
            .unwrap();
        let expected_json = serde_json::to_value(&expected).unwrap();
        assert_eq!(
            serde_json::to_value(state.team_registry.get("test").unwrap()).unwrap(),
            expected_json
        );
        assert_ne!(expected.team_id, saved.team_id);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        server.reset().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/teams/snapshots/snap-server-identity",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&snapshot))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .expect(2)
            .mount(&server)
            .await;
        for command in ["restore test snap-server-identity", "snapshot test failed"] {
            assert!(
                super::handle_team_command(command, &api, None, &mut state)
                    .await
                    .is_err()
            );
            assert_eq!(
                serde_json::to_value(state.team_registry.get("test").unwrap()).unwrap(),
                expected_json
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        server.reset().await;

        let mut wrong_owner = saved.clone();
        wrong_owner.user_id = "another-owner".into();
        for (team_name, definition_json) in [
            ("test", None),
            ("test", Some("{".into())),
            ("test", Some(serde_json::to_string(&wrong_owner).unwrap())),
            ("another-team", Some(serde_json::to_string(&saved).unwrap())),
        ] {
            let mut invalid = snapshot.clone();
            invalid["team_name"] = serde_json::json!(team_name);
            invalid["team_definition_json"] = serde_json::json!(definition_json);
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(
                    "/teams/snapshots/snap-server-identity",
                ))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&invalid))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                super::handle_team_command(
                    "restore test snap-server-identity",
                    &api,
                    None,
                    &mut state
                )
                .await
                .is_err()
            );
            assert_eq!(
                serde_json::to_value(state.team_registry.get("test").unwrap()).unwrap(),
                expected_json
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            server.reset().await;
        }
    }

    // ── Coordination tests ──────────────────────────────────────────

    fn make_team(roles: &[&str]) -> Team {
        let now = "2024-01-01T00:00:00Z".to_string();
        Team {
            team_id: uuid::Uuid::new_v4().to_string(),
            user_id: "u".into(),
            name: "test".into(),
            description: "test team".into(),
            members: roles
                .iter()
                .map(|r| TeamMember {
                    role: r.to_string(),
                    agent_id: None,
                    system_prompt: Some(format!("{r} agent")),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: false,
                    max_delegation_depth: 0,
                    ..Default::default()
                })
                .collect(),
            context: HashMap::new(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    // ── Snapshot tests ─────────────────────────────────

    #[test]
    fn git_head_sha_returns_some_in_git_repo() {
        // This test runs inside a git repo, so should return Some
        let sha = git_head_sha();
        assert!(sha.is_some(), "Expected Some(sha) in a git repo");
        let sha = sha.unwrap();
        assert!(sha.len() >= 7, "SHA too short: {}", sha);
    }

    #[serial_test::serial]
    #[test]
    fn team_subcommands_hint_mentions_run_and_restore() {
        let hint = team_subcommands_hint();
        assert!(hint.contains("run"));
        assert!(hint.contains("restore"));
        assert!(hint.contains("help"));
    }

    // ── New feature tests ───────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn native_team_lead_selection_uses_permissions_and_one_configuration_read() {
        let _creds_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                access_token: Some("test-token".into()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();
        let server = wiremock::MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        for (permissions, requested, expected) in [
            ([false, true], None, Some("member-1")),
            ([false, false], None, None),
            ([true, true], None, None),
            ([false, true], Some("member-0"), Some("member-0")),
            ([true, true], Some("member-1"), Some("member-1")),
            ([false, true], Some("missing"), None),
            ([false, true], Some(""), None),
        ] {
            let mut team = make_team(&["coordinator-looking", "ordinary-looking"]);
            for (index, member) in team.members.iter_mut().enumerate() {
                member.agent_id = Some(format!("member-{index}"));
                member.can_delegate = permissions[index];
                member.max_delegation_depth = u32::from(permissions[index]);
            }
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/teams/test"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&team))
                .expect(1)
                .mount(&server)
                .await;
            let result =
                super::resolve_team_run_chat_request(&api, None, "test", requested, " task ").await;
            match expected {
                Some(id) => {
                    let request = result.unwrap();
                    assert_eq!(request.selection.team_id, team.team_id);
                    assert_eq!(request.selection.lead_agent_id.as_deref(), Some(id));
                    assert_eq!(request.message, "task");
                }
                None => assert!(
                    result.is_err(),
                    "ambiguous or invalid selection must not launch"
                ),
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            server.reset().await;
        }
    }

    #[test]
    fn merge_from_store_replaces_stale_same_name() {
        use astra_services::team_persistence::{TeamDefinition, TeamMemberDef};
        let mut reg = TeamRegistry::new();

        let foreign = TeamDefinition {
            team_id: "foreign-id".into(),
            user_id: "u".into(),
            name: "review".into(),
            description: "foreign review".into(),
            members: vec![],
            context: HashMap::new(),
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        };
        let mut stale = foreign.clone();
        stale.team_id = "stale-id".into();
        stale.description = "stale local projection".into();
        reg.merge_from_store(vec![stale]);
        let custom = TeamDefinition {
            team_id: "custom-id".into(),
            user_id: "u".into(),
            name: "from-store".into(),
            description: "loaded from store".into(),
            members: vec![TeamMemberDef {
                role: "worker".into(),
                agent_id: None,
                system_prompt: Some("does work".into()),
                skills: vec![],
                model_selection: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
                ..Default::default()
            }],
            context: HashMap::new(),
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        };

        reg.merge_from_store(vec![foreign, custom]);

        // Remote persistence is authoritative over the stale projection.
        let review = reg.get("review").unwrap();
        assert_eq!(review.team_id, "foreign-id");

        // "from-store" should be loaded
        let loaded = reg.get("from-store").unwrap();
        assert_eq!(loaded.team_id, "custom-id");
        assert_eq!(loaded.members.len(), 1);
        assert_eq!(
            loaded.members[0].system_prompt.as_deref(),
            Some("does work")
        );
    }

    #[test]
    fn store_loaded_flag_default_false() {
        let reg = TeamRegistry::new();
        assert!(!reg.store_loaded);
    }
}
