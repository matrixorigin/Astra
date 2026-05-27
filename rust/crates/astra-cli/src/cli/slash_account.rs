use super::*;
use crate::cli::agent_runtime::initialize_multi_agent_runtime;
use crate::cli::session_runtime::current_access_token;
use crate::post_auth_cloud_resync;
use crate::{cli_dim, cli_err, cli_ok, cli_section};

async fn refresh_auth_runtime(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut crate::SessionState,
) {
    if let Some(token) = current_access_token(profile) {
        clear_auth_runtime(state);
        initialize_multi_agent_runtime(state, api, token, profile).await;
    }
}

fn clear_auth_runtime(state: &mut crate::SessionState) {
    state.delegation_engine = None;
    state.agent_spawner = None;
    state.root_mailbox = None;
    state.pending_idle_agent_messages.clear();
}

fn clear_local_auth_state(profile: Option<&str>, state: &mut crate::SessionState) {
    let _ = crate::cli::auth_flow::clear_profile_auth(profile);
    clear_auth_runtime(state);
}

pub(crate) async fn handle_account_command(
    cmd: &str,
    arg: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut crate::SessionState,
) -> Result<(), String> {
    match cmd {
        "/register" => {
            cli_section!("Register a new account");
            eprintln!();
            let username = prompt_or("Username", None)?;
            let email = prompt_or("Email   ", None)?;
            let password = prompt_password_masked("Password", None)?;
            match do_register(api, profile, &username, &email, &password).await {
                Ok(_) => {
                    cli_ok!("Registered and logged in");
                    post_auth_cloud_resync(profile, state).await;
                    refresh_auth_runtime(api, profile, state).await;
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
    use super::*;
    use crate::cli::cli_utils::{CredentialsFile, Profile};

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
        crate::save_credentials(&creds).unwrap();

        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let mut state = crate::SessionState::default();
        let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
            std::sync::Arc::new(astra_messaging::InProcessTransport::new()),
            std::sync::Arc::new(
                astra_runtime::server::delegation::engine::DelegationTracker::new(),
            ),
        ));
        let root_addr = astra_messaging::AgentAddress::new("run-root", "root");
        state.root_mailbox = Some(router.register(root_addr.clone(), None).await.unwrap());
        state.pending_idle_agent_messages.push(std::sync::Arc::new(
            astra_messaging::AgentMessage::new(
                astra_messaging::AgentAddress::new("run-worker", "worker"),
                astra_messaging::MessageTarget::Direct { address: root_addr },
                astra_messaging::MessagePayload::Text {
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
        let mut state = crate::SessionState::default();
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
        let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
            std::sync::Arc::new(astra_messaging::InProcessTransport::new()),
            std::sync::Arc::new(
                astra_runtime::server::delegation::engine::DelegationTracker::new(),
            ),
        ));
        let root_addr = astra_messaging::AgentAddress::new("run-root", "root");
        state.root_mailbox = Some(router.register(root_addr.clone(), None).await.unwrap());
        state.pending_idle_agent_messages.push(std::sync::Arc::new(
            astra_messaging::AgentMessage::new(
                astra_messaging::AgentAddress::new("run-worker", "worker"),
                astra_messaging::MessageTarget::Direct { address: root_addr },
                astra_messaging::MessagePayload::Text {
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
        crate::save_credentials(&creds).unwrap();

        let mut state = crate::SessionState::default();
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

        clear_local_auth_state(None, &mut state);

        let creds = crate::load_credentials();
        let profile = creds.profiles.get("default").unwrap();
        assert!(profile.access_token.is_none());
        assert!(profile.refresh_token.is_none());
        assert!(profile.last_session_id.is_none());
        assert_eq!(profile.memoria_api_key.as_deref(), Some("mem-key"));
        assert!(state.delegation_engine.is_none());
        assert!(state.agent_spawner.is_none());
    }
}
