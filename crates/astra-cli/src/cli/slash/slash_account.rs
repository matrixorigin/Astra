use crate::cli::agent_runtime::initialize_multi_agent_runtime;
use crate::cli::auth_flow::{do_login, do_register};
use crate::cli::cli_config::cli_utils::{
    load_credentials, persist_profile_memoria_api_key, profile_name, prompt_or,
    prompt_password_masked,
};
use crate::cli::session::session_runtime::{current_access_token, ensure_state_default_model};
use crate::cli::session::session_state::SessionState;
use crate::post_auth_cloud_resync;
use crate::{cli_dim, cli_err, cli_ok, cli_section, cli_warn};

async fn refresh_auth_runtime(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
) {
    if let Some(token) = current_access_token(profile) {
        clear_auth_runtime(state).await;
        initialize_multi_agent_runtime(state, api, token, profile).await;
    }
}

const AUTH_RUNTIME_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const AUTH_RUNTIME_REPLACED_REASON: &str = "authentication runtime was replaced";

async fn clear_auth_runtime(state: &mut SessionState) {
    // Authentication changes are a runtime ownership boundary just like TUI
    // exit and session rebind. Retire the old task tree before discarding its
    // handles; direct `Option::take` used to strand local agents whenever a
    // user logged out or switched accounts mid-run.
    if let Some(spawner) = state.agent_spawner.take() {
        spawner
            .shutdown_and_wait_with_reason(AUTH_RUNTIME_SHUTDOWN_WAIT, AUTH_RUNTIME_REPLACED_REASON)
            .await;
    }
    state.delegation_engine = None;
    state.root_mailbox = None;
}

async fn clear_local_auth_state(profile: Option<&str>, state: &mut SessionState) {
    // Keep credentials available while cancellation/finalization converges;
    // some executors need the current token to persist their terminal state.
    clear_auth_runtime(state).await;
    let _ = crate::cli::auth_flow::clear_profile_auth(profile);
}

pub(crate) async fn handle_account_command(
    cmd: &str,
    arg: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
) -> Result<(), String> {
    match cmd {
        "/register" => {
            cli_section!("Register a new account");
            eprintln!();
            let username = prompt_or("Username", None)?;
            let email = prompt_or("Email   ", None)?;
            let password = prompt_password_masked("Password", None)?;
            match do_register(api, profile, &username, &email, &password).await {
                Ok(token) => {
                    cli_ok!("Registered and logged in");
                    let sync_report = post_auth_cloud_resync(profile, state).await;
                    if let Some(notice) = sync_report.user_notice() {
                        cli_warn!("{}", notice);
                    }
                    if let Some(model) = ensure_state_default_model(api, &token, state).await {
                        crate::cli::slash::slash_config::set_active_model_for_display(Some(
                            model.clone(),
                        ));
                        cli_ok!("Default model: {}", model);
                    }
                    refresh_auth_runtime(api, profile, state).await;
                }
                Err(e) => cli_err!("Register failed: {}", e),
            }
        }

        "/login" => {
            let username = prompt_or("Username", None)?;
            let password = prompt_password_masked("Password", None)?;
            match do_login(api, profile, &username, &password).await {
                Ok(token) => {
                    cli_ok!("Logged in");
                    let sync_report = post_auth_cloud_resync(profile, state).await;
                    if let Some(notice) = sync_report.user_notice() {
                        cli_warn!("{}", notice);
                    }
                    if let Some(model) = ensure_state_default_model(api, &token, state).await {
                        crate::cli::slash::slash_config::set_active_model_for_display(Some(
                            model.clone(),
                        ));
                        cli_ok!("Default model: {}", model);
                    }
                    refresh_auth_runtime(api, profile, state).await;
                }
                Err(e) => cli_err!("Login failed: {}", e),
            }
        }

        "/logout" => {
            let creds = load_credentials();
            let pname = profile_name(profile, &creds);
            if let Some(p) = creds.profiles.get(&pname).cloned()
                && let Some(refresh) = p.refresh_token
            {
                let _ = api
                    .post_auth_logout_json(&serde_json::json!({ "refresh_token": refresh }))
                    .await;
            }
            clear_local_auth_state(profile, state).await;
            cli_ok!("Logged out");
        }

        "/memory-setup" => {
            if arg.is_empty() {
                cli_dim!("Usage: /memory-setup <api_key>");
                cli_dim!(
                    "Get a key from Memoria: curl -X POST http://localhost:8100/auth/keys -H 'Authorization: Bearer <master_key>' -H 'Content-Type: application/json' -d '{{\"user_id\":\"<user>\",\"name\":\"astra\"}}'"
                );
            } else {
                let _ = persist_profile_memoria_api_key(profile, arg);
                cli_ok!(
                    "Memoria API key saved (CLI memory traffic now goes through the Astra server)"
                );
            }
        }
        _ => unreachable!("unexpected account command: {cmd}"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{clear_auth_runtime, clear_local_auth_state, refresh_auth_runtime};
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };

    #[serial_test::serial]
    #[tokio::test]
    async fn refresh_auth_runtime_replaces_stale_mailbox_state() {
        let _creds_dir = crate::tests::isolate_credentials();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = crate::cli::session::session_state::SessionState::default();
        let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
            std::sync::Arc::new(astra_messaging::InProcessTransport::new()),
            std::sync::Arc::new(
                astra_runtime::server::delegation::engine::DelegationTracker::new(),
            ),
        ));
        let root_addr = astra_messaging::AgentAddress::new("run-root", "root");
        state.root_mailbox = Some(router.register(root_addr, None).await.unwrap());
        assert!(state.delegation_engine.is_none());
        assert!(state.agent_spawner.is_none());
        assert!(state.root_mailbox.is_some());

        refresh_auth_runtime(&api, None, &mut state).await;

        assert!(state.delegation_engine.is_some());
        assert!(state.agent_spawner.is_some());
        assert!(state.root_mailbox.is_none());
    }

    #[tokio::test]
    async fn clear_auth_runtime_drops_multi_agent_runtime() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.delegation_engine = Some(std::sync::Arc::new(
            astra_runtime::server::delegation::engine::DelegationEngine::with_executor(
                std::sync::Arc::new(tokio::sync::RwLock::new(
                    astra_services::AgentProfileRegistry::new(),
                )),
                std::sync::Arc::new(astra_runtime::server::run::engine::RunEngine::new(
                    std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default()),
                )),
                std::sync::Arc::new(
                    astra_runtime::server::delegation::engine::DelegationTracker::new(),
                ),
                std::sync::Arc::new(astra_runtime::server::delegation::engine::StubSubRunExecutor),
            ),
        ));
        let spawner = std::sync::Arc::new(astra_runtime::orchestration::DynamicAgentSpawner::new(
            std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
                std::sync::Arc::new(astra_messaging::InProcessTransport::new()),
                std::sync::Arc::new(
                    astra_runtime::server::delegation::engine::DelegationTracker::new(),
                ),
            )),
        ));
        state.agent_spawner = Some(std::sync::Arc::clone(&spawner));
        let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
            std::sync::Arc::new(astra_messaging::InProcessTransport::new()),
            std::sync::Arc::new(
                astra_runtime::server::delegation::engine::DelegationTracker::new(),
            ),
        ));
        let root_addr = astra_messaging::AgentAddress::new("run-root", "root");
        state.root_mailbox = Some(router.register(root_addr, None).await.unwrap());

        clear_auth_runtime(&mut state).await;

        assert!(state.delegation_engine.is_none());
        assert!(state.agent_spawner.is_none());
        assert!(state.root_mailbox.is_none());

        let rejected = spawner
            .spawn(
                astra_runtime::orchestration::SpawnAgentInput {
                    description: "must not survive auth replacement".into(),
                    prompt: "pending work".into(),
                    agent_type: "explore".into(),
                    run_in_background: true,
                    ..Default::default()
                },
                &astra_runtime::orchestration::SpawnContext {
                    parent_run_id: "root".into(),
                    parent_agent_id: "root".into(),
                    resolved_model_name: None,
                    recursion_depth: 0,
                    parent_is_fork_child: false,
                    working_dir: std::path::PathBuf::from("/tmp"),
                    inherited_permissions:
                        astra_runtime::orchestration::InheritedPermissions::auto_approve(),
                    inherited_skills: Vec::new(),
                    live_event_sink: None,
                    client_tool_delivery_tx: None,
                    trace_context: None,
                    spawn_tool_call_id: None,
                    execution_metadata: None,
                    delegation_chain: Vec::new(),
                },
            )
            .await;
        assert!(matches!(
            rejected,
            Err(astra_runtime::orchestration::SpawnError::LifecycleShuttingDown)
        ));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn clear_local_auth_state_clears_credentials_and_runtime() {
        let _creds_dir = crate::tests::isolate_credentials();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("tok".to_string()),
                refresh_token: Some("ref".to_string()),
                last_session_id: Some("sess".to_string()),
                memoria_api_key: Some("mem-key".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let mut state = crate::cli::session::session_state::SessionState::default();
        state.delegation_engine = Some(std::sync::Arc::new(
            astra_runtime::server::delegation::engine::DelegationEngine::with_executor(
                std::sync::Arc::new(tokio::sync::RwLock::new(
                    astra_services::AgentProfileRegistry::new(),
                )),
                std::sync::Arc::new(astra_runtime::server::run::engine::RunEngine::new(
                    std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default()),
                )),
                std::sync::Arc::new(
                    astra_runtime::server::delegation::engine::DelegationTracker::new(),
                ),
                std::sync::Arc::new(astra_runtime::server::delegation::engine::StubSubRunExecutor),
            ),
        ));
        state.agent_spawner = Some(std::sync::Arc::new(
            astra_runtime::orchestration::DynamicAgentSpawner::new(std::sync::Arc::new(
                astra_messaging::AgentMailboxRouter::new(
                    std::sync::Arc::new(astra_messaging::InProcessTransport::new()),
                    std::sync::Arc::new(
                        astra_runtime::server::delegation::engine::DelegationTracker::new(),
                    ),
                ),
            )),
        ));

        clear_local_auth_state(None, &mut state).await;

        let creds = load_credentials();
        let profile = creds.profiles.get("default").unwrap();
        assert!(profile.access_token.is_none());
        assert!(profile.refresh_token.is_none());
        assert!(profile.last_session_id.is_none());
        assert_eq!(profile.memoria_api_key.as_deref(), Some("mem-key"));
        assert!(state.delegation_engine.is_none());
        assert!(state.agent_spawner.is_none());
    }
}
