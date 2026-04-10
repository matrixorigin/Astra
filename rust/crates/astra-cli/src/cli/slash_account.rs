use super::*;
use crate::{cli_dim, cli_err, cli_ok, cli_section, cli_warn};
use crate::{current_access_token, initialize_multi_agent_runtime, post_auth_cloud_resync};

async fn refresh_auth_runtime(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut super::ReplState,
) {
    if let Some(token) = current_access_token(profile) {
        clear_auth_runtime(state);
        initialize_multi_agent_runtime(state, api, token).await;
    }
}

fn clear_auth_runtime(state: &mut super::ReplState) {
    state.delegation_engine = None;
    state.agent_spawner = None;
    state.root_mailbox = None;
    state.pending_idle_agent_messages.clear();
}

fn clear_local_auth_state(profile: Option<&str>, state: &mut super::ReplState) {
    let _ = crate::auth_flow::clear_profile_auth(profile);
    clear_auth_runtime(state);
}

pub(super) async fn handle_account_command(
    cmd: &str,
    arg: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut super::ReplState,
) -> Result<(), String> {
    match cmd {
        "/register" => {
            cli_section!("Register a new account");
            eprintln!();
            let username = prompt_or("Username", None)?;
            let email = prompt_or("Email   ", None)?;
            let password = prompt_password_masked("Password", None)?;
            match do_register(api, &username, &email, &password).await {
                Ok(_) => {
                    cli_ok!("Registered! Logging in…");
                    match do_login(api, profile, &username, &password).await {
                        Ok(_) => {
                            cli_ok!("Logged in");
                            post_auth_cloud_resync(profile, state).await;
                            refresh_auth_runtime(api, profile, state).await;
                        }
                        Err(e) => {
                            clear_local_auth_state(profile, state);
                            cli_err!("Login failed: {}", e);
                            cli_warn!("Registration succeeded, but no user is logged in now.");
                        }
                    }
                }
                Err(e) => cli_err!("Register failed: {}", e),
            }
        }

        "/login" => {
            let username = prompt_or("Username", None)?;
            let password = prompt_password_masked("Password", None)?;
            match do_login(api, profile, &username, &password).await {
                Ok(_) => {
                    cli_ok!("Logged in");
                    post_auth_cloud_resync(profile, state).await;
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
            clear_local_auth_state(profile, state);
            cli_ok!("Logged out");
        }

        "/memory-setup" => {
            if arg.is_empty() {
                cli_dim!("Usage: /memory-setup <api_key>");
                cli_dim!(
                    "Get a key from Memoria: curl -X POST http://localhost:8100/auth/keys -H 'Authorization: Bearer <master_key>' -H 'Content-Type: application/json' -d '{{\"user_id\":\"<user>\",\"name\":\"astra\"}}'"
                );
            } else {
                let mut creds = load_credentials();
                let pname = profile_name(profile, &creds);
                let p = creds.profiles.entry(pname).or_default();
                p.memoria_api_key = Some(arg.to_string());
                let _ = save_credentials(&creds);
                // SAFETY: This runs during single-threaded REPL command processing.
                // No concurrent threads read MEMORIA_API_KEY at this point.
                unsafe {
                    std::env::set_var("MEMORIA_API_KEY", arg);
                }
                cli_ok!("Memoria API key saved");
            }
        }
        _ => unreachable!("unexpected account command: {cmd}"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_utils::{CredentialsFile, Profile};

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
        super::save_credentials(&creds).unwrap();

        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = super::ReplState::default();
        let router = std::sync::Arc::new(astra_runtime::messaging::AgentMailboxRouter::new(
            std::sync::Arc::new(astra_runtime::messaging::InProcessTransport::new()),
            std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new()),
        ));
        let root_addr = astra_runtime::messaging::AgentAddress::new("run-root", "root");
        state.root_mailbox = Some(router.register(root_addr.clone(), None).await.unwrap());
        state.pending_idle_agent_messages.push(std::sync::Arc::new(
            astra_runtime::messaging::AgentMessage::new(
                astra_runtime::messaging::AgentAddress::new("run-worker", "worker"),
                astra_runtime::messaging::MessageTarget::Direct { address: root_addr },
                astra_runtime::messaging::MessagePayload::Text {
                    content: "stale".to_string(),
                    summary: None,
                },
            ),
        ));
        assert!(state.delegation_engine.is_none());
        assert!(state.agent_spawner.is_none());
        assert!(state.root_mailbox.is_some());
        assert_eq!(state.pending_idle_agent_messages.len(), 1);

        refresh_auth_runtime(&api, None, &mut state).await;

        assert!(state.delegation_engine.is_some());
        assert!(state.agent_spawner.is_some());
        assert!(state.root_mailbox.is_none());
        assert!(state.pending_idle_agent_messages.is_empty());
    }

    #[tokio::test]
    async fn clear_auth_runtime_drops_multi_agent_runtime() {
        let mut state = super::ReplState::default();
        state.delegation_engine = Some(std::sync::Arc::new(
            astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
                std::sync::Arc::new(tokio::sync::RwLock::new(
                    astra_services::AgentProfileRegistry::new(),
                )),
                std::sync::Arc::new(astra_runtime::server::run_engine::RunEngine::new(
                    std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default()),
                )),
                std::sync::Arc::new(
                    astra_runtime::server::delegation_engine::DelegationTracker::new(),
                ),
                std::sync::Arc::new(astra_runtime::server::delegation_engine::StubSubRunExecutor),
            ),
        ));
        state.agent_spawner = Some(std::sync::Arc::new(
            astra_runtime::orchestration::DynamicAgentSpawner::new(std::sync::Arc::new(
                astra_runtime::messaging::AgentMailboxRouter::new(
                    std::sync::Arc::new(astra_runtime::messaging::InProcessTransport::new()),
                    std::sync::Arc::new(
                        astra_runtime::server::delegation_engine::DelegationTracker::new(),
                    ),
                ),
            )),
        ));
        let router = std::sync::Arc::new(astra_runtime::messaging::AgentMailboxRouter::new(
            std::sync::Arc::new(astra_runtime::messaging::InProcessTransport::new()),
            std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new()),
        ));
        let root_addr = astra_runtime::messaging::AgentAddress::new("run-root", "root");
        state.root_mailbox = Some(router.register(root_addr.clone(), None).await.unwrap());
        state.pending_idle_agent_messages.push(std::sync::Arc::new(
            astra_runtime::messaging::AgentMessage::new(
                astra_runtime::messaging::AgentAddress::new("run-worker", "worker"),
                astra_runtime::messaging::MessageTarget::Direct { address: root_addr },
                astra_runtime::messaging::MessagePayload::Text {
                    content: "queued".to_string(),
                    summary: None,
                },
            ),
        ));

        clear_auth_runtime(&mut state);

        assert!(state.delegation_engine.is_none());
        assert!(state.agent_spawner.is_none());
        assert!(state.root_mailbox.is_none());
        assert!(state.pending_idle_agent_messages.is_empty());
    }

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
        super::save_credentials(&creds).unwrap();

        let mut state = super::ReplState::default();
        state.delegation_engine = Some(std::sync::Arc::new(
            astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
                std::sync::Arc::new(tokio::sync::RwLock::new(
                    astra_services::AgentProfileRegistry::new(),
                )),
                std::sync::Arc::new(astra_runtime::server::run_engine::RunEngine::new(
                    std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default()),
                )),
                std::sync::Arc::new(
                    astra_runtime::server::delegation_engine::DelegationTracker::new(),
                ),
                std::sync::Arc::new(astra_runtime::server::delegation_engine::StubSubRunExecutor),
            ),
        ));
        state.agent_spawner = Some(std::sync::Arc::new(
            astra_runtime::orchestration::DynamicAgentSpawner::new(std::sync::Arc::new(
                astra_runtime::messaging::AgentMailboxRouter::new(
                    std::sync::Arc::new(astra_runtime::messaging::InProcessTransport::new()),
                    std::sync::Arc::new(
                        astra_runtime::server::delegation_engine::DelegationTracker::new(),
                    ),
                ),
            )),
        ));

        clear_local_auth_state(None, &mut state);

        let creds = super::load_credentials();
        let profile = creds.profiles.get("default").unwrap();
        assert!(profile.access_token.is_none());
        assert!(profile.refresh_token.is_none());
        assert!(profile.last_session_id.is_none());
        assert_eq!(profile.memoria_api_key.as_deref(), Some("mem-key"));
        assert!(state.delegation_engine.is_none());
        assert!(state.agent_spawner.is_none());
    }
}
