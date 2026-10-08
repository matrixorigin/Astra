use super::cli_config::cli_args::NativeAuthCommand;
use astra_credentials::native::{self, NativeStore};
use std::io::Write;
use std::sync::{Arc, RwLock};

#[derive(Debug)]
pub(crate) struct Binding {
    store: NativeStore,
    session: native::NativeSession,
}

static ACTIVE: RwLock<Option<Arc<Binding>>> = RwLock::new(None);

#[cfg(test)]
pub(crate) fn binding_for_test(store: NativeStore, session: native::NativeSession) -> Arc<Binding> {
    Arc::new(Binding { store, session })
}

#[cfg(test)]
pub(crate) struct ActiveBindingGuard(Option<Arc<Binding>>);

#[cfg(test)]
impl Drop for ActiveBindingGuard {
    fn drop(&mut self) {
        *ACTIVE.write().expect("native identity lock poisoned") = self.0.take();
    }
}

#[cfg(test)]
pub(crate) fn install_active_for_test(binding: Arc<Binding>) -> ActiveBindingGuard {
    let previous = ACTIVE
        .write()
        .expect("native identity lock poisoned")
        .replace(binding);
    ActiveBindingGuard(previous)
}

pub(crate) fn active() -> Option<Arc<Binding>> {
    ACTIVE
        .read()
        .expect("native identity lock poisoned")
        .clone()
}

impl Binding {
    pub(crate) fn endpoint(&self) -> &str {
        &self.session.environment.astra_url
    }

    pub(crate) fn profile_name(&self) -> String {
        format!(
            "moi-{}-{}",
            self.session.environment.key(),
            self.session.astra_user_id
        )
    }

    fn check_identity(
        &self,
        current: native::NativeSession,
    ) -> Result<native::NativeSession, String> {
        if current.generation != self.session.generation
            || current.environment != self.session.environment
            || current.subject != self.session.subject
        {
            return Err("MOI account or environment changed; restart Astra".into());
        }
        Ok(current)
    }

    /// Local identity binding does not require a usable access token.
    pub(crate) fn account_id(&self) -> Result<String, String> {
        Ok(self.check_identity(self.store.current()?)?.astra_user_id)
    }

    pub(crate) fn snapshot(&self) -> Result<native::NativeSession, String> {
        let current = self.check_identity(self.store.current()?)?;
        if current.refresh_pending {
            return Err("MOI token rotation was interrupted; run astra login".into());
        }
        Ok(current)
    }

    /// Same identity check as [`snapshot`], without failing when a rotation is pending.
    /// The file lock runs on the blocking pool.
    pub(crate) async fn snapshot_off_runtime(&self) -> Result<native::NativeSession, String> {
        self.check_identity(self.store.current_off_runtime().await?)
    }

    pub(crate) async fn access_token(&self) -> Result<String, native::CredentialFailure> {
        let credential = self
            .store
            .credential("astra", Some(&self.session.generation))
            .await?;
        if credential.endpoint != self.session.environment.astra_url
            || credential.subject != self.session.subject
            || credential.environment != self.session.environment.key()
        {
            return Err(native::CredentialFailure::AccountChanged);
        }
        Ok(credential.access_token)
    }
}

/// Resolve native auth before startup launches cloud work. The progress timer
/// never cancels the credential future or starts another refresh.
pub(crate) async fn startup_access_token(binding: &Binding) -> Result<String, native::AccessMiss> {
    let pending = binding.access_token();
    tokio::pin!(pending);
    let result = tokio::select! {
        biased;
        result = &mut pending => result,
        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
            eprintln!("Refreshing your sign-in…");
            pending.await
        }
    };
    result.map_err(|error| error.access_miss())
}

impl astra_thin_client::client::BearerProvider for Binding {
    fn token(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<String, astra_thin_client::ThinClientError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async {
            self.access_token().await.map_err(|error| {
                astra_thin_client::ThinClientError::InvalidInput(error.to_string())
            })
        })
    }
}

/// An explicit legacy profile retains Memoria/self-hosting behavior. Without
/// one, selecting MOI is sticky, including after logout or corrupt state.
pub(crate) fn bind_process(
    base: &mut String,
    explicit_api: bool,
    legacy_profile: Option<&str>,
    login: bool,
) -> Result<Option<Arc<Binding>>, String> {
    if legacy_profile.is_some() {
        return Ok(None);
    }
    let store = NativeStore::new()?;
    if !store.configured()? {
        return Ok(None);
    }
    if login {
        if !explicit_api && let Some(environment) = store.selected_environment()? {
            *base = environment.astra_url;
        }
        return Ok(None);
    }
    let session = store.current()?;
    session.environment.validate()?;
    if explicit_api && base.trim_end_matches('/') != session.environment.astra_url {
        return Err("Astra endpoint differs from the MOI login; sign in there first, or select an explicit legacy --profile".into());
    }
    if std::env::var_os("ASTRA_ACCESS_TOKEN").is_some() {
        return Err(
            "cannot mix ASTRA_ACCESS_TOKEN with MOI login; select an explicit legacy --profile"
                .into(),
        );
    }
    *base = session.environment.astra_url.clone();
    let binding = Arc::new(Binding { store, session });
    *ACTIVE
        .write()
        .map_err(|_| "native identity lock poisoned")? = Some(binding.clone());
    Ok(Some(binding))
}

/// Only called at an explicit, quiesced authentication boundary. Existing
/// transports keep their old generation-pinned Arc and cannot switch account.
pub(crate) fn bind_after_login(
    api: &astra_thin_client::ThinClient,
) -> Result<astra_thin_client::ThinClient, String> {
    let mut base = api.api_origin();
    let binding = bind_process(&mut base, true, None, false)?
        .ok_or("UC login did not publish credentials")?;
    crate::cli::cli_config::cli_utils::install_cli_profile_identity(
        binding.profile_name(),
        Some(binding.account_id()?),
    )?;
    Ok(api.clone().with_bearer_provider(binding))
}

pub(crate) fn projected_credentials() -> Result<Option<astra_credentials::CredentialsFile>, String>
{
    let Some(binding) = active() else {
        return Ok(None);
    };
    let session = binding.check_identity(binding.store.current()?)?;
    let name = binding.profile_name();
    // Only local session metadata lives in the old profile file. Neither a
    // refresh token nor a second persisted access token is copied there.
    let metadata = astra_credentials::CredentialStore::new()
        .load()
        .map_err(|_| "cannot read local Astra session metadata")?;
    let last_session_id = metadata
        .profiles
        .get(&name)
        .and_then(|p| p.last_session_id.clone());
    let mut credentials = astra_credentials::CredentialsFile {
        current_profile: Some(name.clone()),
        ..Default::default()
    };
    credentials.profiles.insert(
        name,
        astra_credentials::Profile {
            username: Some(session.subject),
            account_id: Some(session.astra_user_id),
            // Resume metadata and account identity remain readable during
            // rotation, but the unsettled access token must not escape.
            access_token: (!session.refresh_pending).then_some(session.access_token),
            last_session_id,
            ..Default::default()
        },
    );
    Ok(Some(credentials))
}

pub(crate) async fn command(command: &NativeAuthCommand) -> Result<(), String> {
    let store = NativeStore::new()?;
    match command {
        NativeAuthCommand::Workspace { id, clear } => {
            let session = store.current()?;
            if *clear {
                return store.select_workspace(&session, None);
            }
            let credential = store.credential("moi", Some(&session.generation)).await?;
            let bootstrap = super::auth_flow::uc::moi_bootstrap(
                &native::http_client()?,
                &session.environment,
                &credential.access_token,
            )
            .await?;
            if bootstrap.issuer != session.environment.issuer
                || bootstrap.subject != session.subject
                || bootstrap.session_id != session.session_id
            {
                return Err("MOI workspace response identity mismatch".into());
            }
            super::auth_flow::uc::choose_workspace(&store, &session, bootstrap, id.as_deref())
        }
        NativeAuthCommand::Status { json } => {
            let configured = store.configured()?;
            let status = if !configured {
                serde_json::json!({"version": 1, "state": "not_configured"})
            } else {
                match store.status().await {
                    Ok((session, status)) => {
                        serde_json::json!({"version": 1, "state": status,
                        "environment": session.environment.key(), "issuer": session.environment.issuer,
                        "astra_url": session.environment.astra_url, "moi_url": session.environment.moi_url,
                        "subject": session.subject, "generation": session.generation, "expires_at": session.expires_at,
                        "workspace_id": session.workspace_id, "role_id": session.role_id})
                    }
                    Err(error) if error == "MOI session is logged out; run astra login" => {
                        serde_json::json!({"version": 1, "state": "signed_out"})
                    }
                    Err(error) => return Err(error),
                }
            };
            super::stream::output_sink::write_stdout_line(&render_status(&status, *json))
                .map_err(|_| "cannot write native authentication status")?;
            Ok(())
        }
        NativeAuthCommand::Credential {
            target,
            output_fd,
            generation,
        } => {
            let mut pipe = credential_pipe(*output_fd)?;
            let value = store.credential(target, generation.as_deref()).await?;
            serde_json::to_writer(&mut pipe, &value).map_err(|_| "cannot write credential pipe")?;
            pipe.flush()
                .map_err(|_| "cannot flush credential pipe".into())
        }
    }
}

fn render_status(status: &serde_json::Value, json: bool) -> String {
    if json {
        return status.to_string();
    }
    match status["state"].as_str() {
        Some("signed_in" | "refresh_in_progress" | "reauthentication_required") => {
            let heading = match status["state"].as_str() {
                Some("signed_in") => "Signed in to MOI.",
                Some("refresh_in_progress") => {
                    "Sign-in refresh is in progress. Please wait; no login is needed."
                }
                _ => "Sign-in needs to be renewed. Run astra login.",
            };
            format!(
                "{heading}\nAccount: {}\nAstra: {}\nMOI: {}\nWorkspace: {}",
                status["subject"].as_str().unwrap_or(""),
                status["astra_url"].as_str().unwrap_or(""),
                status["moi_url"].as_str().unwrap_or(""),
                status["workspace_id"].as_str().unwrap_or("not selected"),
            )
        }
        Some("signed_out") => "Signed out of MOI. Run astra login to sign in.".into(),
        _ => "MOI sign-in is not configured. Run astra login to get started.".into(),
    }
}

#[cfg(unix)]
fn credential_pipe(fd: i32) -> Result<std::fs::File, String> {
    use std::os::fd::FromRawFd;
    if fd < 3 {
        return Err(
            "credential output requires a private inherited pipe, not standard output".into(),
        );
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // Validate the inherited descriptor before adopting a duplicate. A terminal,
    // regular file, socket or standard output can never receive credentials.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err("invalid credential pipe descriptor".into());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err("credential output is not a private pipe".into());
    }
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err("cannot duplicate credential pipe".into());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(duplicate) })
}

#[cfg(not(unix))]
fn credential_pipe(_: i32) -> Result<std::fs::File, String> {
    Err("native authentication supports macOS and Linux".into())
}

pub(crate) async fn logout() -> Result<(), String> {
    if let Some(session) = NativeStore::new()?.logout()? {
        native::revoke(&session.environment, &session.refresh_token).await?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_session() -> native::NativeSession {
    let issuer = "https://uc.example.test/realms/moi";
    native::NativeSession {
        environment: native::Environment {
            issuer: issuer.into(),
            astra_url: "https://astra.example.test".into(),
            moi_url: "https://moi.example.test/newmoi".into(),
            authorization_endpoint: format!("{issuer}/protocol/openid-connect/auth"),
            token_endpoint: format!("{issuer}/protocol/openid-connect/token"),
            revocation_endpoint: format!("{issuer}/protocol/openid-connect/revoke"),
            jwks_uri: format!("{issuer}/protocol/openid-connect/certs"),
        },
        generation: String::new(),
        subject: "account-a".into(),
        session_id: "session-a".into(),
        astra_user_id: "astra-a".into(),
        moi_principal_id: "moi-a".into(),
        catalog_user_id: "catalog-a".into(),
        access_token: "test-access-a".into(),
        refresh_token: "test-refresh-a".into(),
        expires_at: native::unix_now().unwrap() + 3600,
        workspace_id: None,
        role_id: None,
        refresh_pending: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_respects_json_and_renders_all_human_states_without_credentials() {
        for (state, expected) in [
            ("not_configured", "not configured"),
            ("signed_out", "Signed out"),
            ("signed_in", "Signed in"),
            ("refresh_in_progress", "no login is needed"),
            ("reauthentication_required", "needs to be renewed"),
        ] {
            let status = serde_json::json!({
                "version": 1, "state": state, "subject": "account-a",
                "astra_url": "https://astra.example.test", "moi_url": "https://moi.example.test",
                "workspace_id": null
            });
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&render_status(&status, true)).unwrap(),
                status
            );
            let human = render_status(&status, false);
            assert!(human.contains(expected), "{human}");
            assert!(!human.starts_with('{'));
            if matches!(
                state,
                "signed_in" | "refresh_in_progress" | "reauthentication_required"
            ) {
                assert!(human.contains("Workspace: not selected"));
                assert!(human.contains("Account: account-a"));
            }
        }
    }

    #[tokio::test]
    async fn login_rebinding_does_not_retarget_existing_bearer_provider() {
        let root = tempfile::tempdir().unwrap();
        let store = NativeStore::with_directory(root.path().join("auth"));
        let original = test_session();
        let (session, _) = store.publish(original.clone()).unwrap();
        let old = Binding {
            store: store.clone(),
            session,
        };
        assert_eq!(old.access_token().await.unwrap(), "test-access-a");
        let mut next = original;
        next.subject = "account-b".into();
        next.astra_user_id = "astra-b".into();
        next.access_token = "test-access-b".into();
        let (session, _) = store.publish(next).unwrap();
        let new = Binding { store, session };
        assert!(old.snapshot().is_err());
        assert!(old.account_id().is_err());
        assert!(old.access_token().await.is_err());
        assert_eq!(new.access_token().await.unwrap(), "test-access-b");
        assert_ne!(old.profile_name(), new.profile_name());
    }

    #[tokio::test]
    async fn startup_identity_accepts_pending_but_credentials_wait_for_settlement() {
        const CHILD: &str = "ASTRA_PENDING_STARTUP_TEST";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "cli::native_auth::tests::startup_identity_accepts_pending_but_credentials_wait_for_settlement", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let _home = crate::test_utils::HomeGuard::temp();
        let _credentials = crate::test_utils::isolate_credentials();
        use fs2::FileExt;
        let root = tempfile::tempdir().unwrap();
        let store = NativeStore::with_directory(root.path().join("auth"));
        let (session, _) = store.publish(test_session()).unwrap();
        let path = root.path().join("auth/auth.json");
        let lock_path = root
            .path()
            .join(format!("auth/refresh-{}.lock", session.environment.key()));
        use std::os::unix::fs::OpenOptionsExt;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(lock_path)
            .unwrap();
        lock.lock_exclusive().unwrap();
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state["sessions"][session.environment.key()]["refresh_pending"] = true.into();
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        let binding = Arc::new(Binding { store, session });
        let _active = install_active_for_test(binding.clone());
        use crate::cli::cli_config::cli_utils::{self, CliProfileIdentityAdmission};
        cli_utils::configure_cli_profile_identity(
            None,
            CliProfileIdentityAdmission::RequireBoundAccount,
        )
        .unwrap();
        assert!(
            cli_utils::cli_owner_auth_snapshot()
                .native_binding
                .is_some()
        );
        let last_session = uuid::Uuid::new_v4().to_string();
        let mut metadata = astra_credentials::CredentialsFile::default();
        metadata.profiles.insert(
            binding.profile_name(),
            astra_credentials::Profile {
                last_session_id: Some(last_session.clone()),
                ..Default::default()
            },
        );
        cli_utils::save_credentials(&metadata).unwrap();
        let projection = projected_credentials().unwrap().unwrap();
        assert!(
            projection.profiles[&binding.profile_name()]
                .access_token
                .is_none()
        );
        assert_eq!(
            projection.profiles[&binding.profile_name()]
                .username
                .as_deref(),
            Some("account-a")
        );
        assert_eq!(cli_utils::stored_last_session_id(None), Some(last_session));
        assert_eq!(binding.account_id().unwrap(), "astra-a");
        assert!(
            binding.snapshot().is_err(),
            "pending must not expose an access token"
        );
        let pending = startup_access_token(&binding);
        tokio::pin!(pending);
        // The progress threshold must not cancel/restart the credential future.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1100), &mut pending)
                .await
                .is_err()
        );
        state["sessions"][binding.session.environment.key()]["refresh_pending"] = false.into();
        state["sessions"][binding.session.environment.key()]["access_token"] =
            "settled-access".into();
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        FileExt::unlock(&lock).unwrap();
        assert_eq!(pending.await.unwrap(), "settled-access");

        state["sessions"][binding.session.environment.key()]["refresh_pending"] = true.into();
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(
            binding.access_token().await.unwrap_err(),
            native::CredentialFailure::RotationInterrupted
        );
        assert_eq!(
            startup_access_token(&binding).await.unwrap_err(),
            native::AccessMiss::ReauthenticationRequired
        );
    }

    #[tokio::test]
    async fn cloud_sync_native_credentials_remain_bound_to_the_selected_origin() {
        // Native process authority is global. Exercise it in a fresh process so
        // unrelated legacy tests never observe this synthetic login.
        const CHILD: &str = "ASTRA_NATIVE_CLOUD_BINDING_TEST";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "cli::native_auth::tests::cloud_sync_native_credentials_remain_bound_to_the_selected_origin", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, path},
        };
        let selected = MockServer::start().await;
        let foreign = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let _home = crate::test_utils::HomeGuard::temp();
        // This child runs exactly one test and restores no shared parent state.
        unsafe {
            std::env::set_var("MOI_AUTH_DIR", root.path().join("auth"));
            std::env::set_var("ASTRA_API_URL", foreign.uri());
        }
        let _legacy_token = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let issuer = "https://uc.example.test/realms/moi";
        let store = NativeStore::new().unwrap();
        let (session, _) = store
            .publish(native::NativeSession {
                environment: native::Environment {
                    issuer: issuer.into(),
                    astra_url: selected.uri(),
                    moi_url: "https://moi.example.test/newmoi".into(),
                    authorization_endpoint: format!("{issuer}/protocol/openid-connect/auth"),
                    token_endpoint: format!("{issuer}/protocol/openid-connect/token"),
                    revocation_endpoint: format!("{issuer}/protocol/openid-connect/revoke"),
                    jwks_uri: format!("{issuer}/protocol/openid-connect/certs"),
                },
                generation: String::new(),
                subject: "account-a".into(),
                session_id: "session-a".into(),
                astra_user_id: "astra-a".into(),
                moi_principal_id: "moi-a".into(),
                catalog_user_id: "catalog-a".into(),
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                expires_at: native::unix_now().unwrap() + 3600,
                workspace_id: None,
                role_id: None,
                refresh_pending: false,
            })
            .unwrap();
        let mut base = selected.uri();
        let binding = bind_process(&mut base, true, None, false).unwrap().unwrap();
        crate::cli::cli_config::cli_utils::install_cli_profile_identity(
            binding.profile_name(),
            Some(session.astra_user_id.clone()),
        )
        .unwrap();
        // A catalog 401 must not refresh an unrelated legacy profile against
        // the selected UC server. Reuse this isolated process/binding fixture.
        {
            use crate::cli::cli_config::cli_utils::{Profile, credential_store, save_credentials};
            let mut credentials = credential_store().load().unwrap();
            credentials.profiles.insert(
                "legacy-refresh-test".into(),
                Profile {
                    access_token: Some("legacy-access".into()),
                    refresh_token: Some("legacy-refresh".into()),
                    ..Default::default()
                },
            );
            save_credentials(&credentials).unwrap();
            assert_eq!(
                credential_store().load().unwrap().profiles["legacy-refresh-test"]
                    .refresh_token
                    .as_deref(),
                Some("legacy-refresh")
            );
            Mock::given(path("/auth/refresh"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user_id": "wrong-identity", "access_token": "wrong-access", "refresh_token": "wrong-refresh"
                })))
                .expect(0).mount(&selected).await;
            let api = astra_thin_client::ThinClient::new(&selected.uri(), None).unwrap();
            assert!(
                !crate::cli::session::session_runtime::attempt_token_refresh(
                    &api,
                    Some("legacy-refresh-test")
                )
                .await
            );
            assert_eq!(binding.access_token().await.unwrap(), "synthetic-access");
            assert_eq!(
                credential_store().load().unwrap().profiles["legacy-refresh-test"]
                    .refresh_token
                    .as_deref(),
                Some("legacy-refresh")
            );
        }
        Mock::given(path("/preferences"))
            .and(header("Authorization", "Bearer synthetic-access"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"preferences":[]})),
            )
            .expect(2)
            .mount(&selected)
            .await;
        assert!(
            crate::cli::cloud_sync::try_cloud_pull(&binding.profile_name())
                .await
                .cloud_reachable
        );
        let snapshot = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
        assert_eq!(
            snapshot.native_binding.as_ref().unwrap().endpoint(),
            selected.uri()
        );
        assert!(snapshot.access_token().await.is_none());
        Mock::given(path("/memory/health"))
            .and(header("Authorization", "Bearer synthetic-access"))
            .respond_with(ResponseTemplate::new(200).set_body_string("memory ready"))
            .expect(2)
            .mount(&selected)
            .await;
        assert_eq!(
            crate::edge_tools::memoria::memoria_health().await.unwrap(),
            "memory ready"
        );
        let _no_env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_API_URL");
        assert_eq!(
            crate::edge_tools::memoria::memoria_health().await.unwrap(),
            "memory ready"
        );
        // Native memory requests must not follow even a same-origin redirect.
        Mock::given(path("/memory/snapshots"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/memory/redirected", selected.uri())),
            )
            .expect(1)
            .mount(&selected)
            .await;
        assert!(
            crate::edge_tools::memoria::memoria_snapshots_list()
                .await
                .is_err()
        );
        assert!(
            !selected
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path() == "/memory/redirected")
        );
        assert!(
            crate::cli::cloud_sync::try_cloud_pull(&binding.profile_name())
                .await
                .cloud_reachable
        );
        assert!(
            crate::cli::preferences_client::pull_all_preferences(
                &foreign.uri(),
                Some("synthetic-access")
            )
            .await
            .is_err()
        );
        store.logout().unwrap();
        assert!(crate::edge_tools::memoria::memoria_health().await.is_err());
        assert!(
            snapshot
                .native_binding
                .unwrap()
                .access_token()
                .await
                .is_err()
        );
        assert!(
            !crate::cli::cloud_sync::try_cloud_pull(&binding.profile_name())
                .await
                .cloud_reachable
        );
        assert!(foreign.received_requests().await.unwrap().is_empty());
    }
}
