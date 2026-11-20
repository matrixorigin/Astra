//! Device-owned renewable ChatGPT connections. Static model credentials are
//! intentionally not used for OAuth rotation. A durable intent fences every
//! refresh across processes, while platform suspension preserves token custody.
use std::{collections::BTreeMap, path::PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::local_models::{
    ensure_private_directory, open_lock, read_private_file, write_atomic_private,
};

pub mod protocol;

pub const ISSUER: &str = "https://auth.openai.com";
pub const API_BASE: &str = "https://api.openai.com/v1";
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const VERSION: u32 = 1;
const MAX_CONNECTIONS: usize = 16;
const MAX_STATE_BYTES: usize = 1024 * 1024;
const REFRESH_LEASE_SECONDS: i64 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("Device ChatGPT storage is unavailable")]
    Storage,
    #[error("Device ChatGPT storage has an invalid owner or format")]
    InvalidStorage,
    #[error("Device ChatGPT connection does not exist")]
    NotFound,
    #[error("Device ChatGPT connection changed; repeat the requested operation")]
    Changed,
    #[error("Device ChatGPT credentials or registration are invalid")]
    InvalidCredentials,
    #[error("Device ChatGPT refresh is already running")]
    RefreshPending,
    #[error("Device ChatGPT refresh outcome is unknown; authorize this connection again")]
    RefreshUnknown,
    #[error("Device ChatGPT connection requires authorization")]
    Reauthorize,
    #[error("Device ChatGPT connection is inactive; enable it from /model")]
    Inactive,
    #[error("Too many stored device ChatGPT registrations; reauthorize an existing connection")]
    Capacity,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").finish_non_exhaustive()
    }
}

impl Tokens {
    pub fn new(access: String, refresh: String, expires_at: i64) -> Result<Self, Error> {
        let value = Self {
            access_token: access,
            refresh_token: refresh,
            expires_at,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), Error> {
        if self.expires_at <= 0
            || [&self.access_token, &self.refresh_token]
                .iter()
                .any(|v| v.is_empty() || v.len() > 65_536 || v.chars().any(char::is_control))
        {
            return Err(Error::InvalidCredentials);
        }
        Ok(())
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub client_id: String,
    pub subject: String,
    pub host_id: String,
}

impl Registration {
    fn validate(&self) -> Result<(), Error> {
        for value in [&self.client_id, &self.subject] {
            if value.is_empty()
                || value.trim() != value
                || value.len() > 255
                || value.chars().any(char::is_control)
            {
                return Err(Error::InvalidCredentials);
            }
        }
        if self.client_id == protocol::DYNAMIC_CLIENT {
            return Err(Error::InvalidCredentials);
        }
        let host = self
            .host_id
            .strip_prefix("urn:uuid:")
            .ok_or(Error::InvalidCredentials)?;
        Uuid::parse_str(host).map_err(|_| Error::InvalidCredentials)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialState {
    Ready,
    PlanDisabled,
    Refreshing { operation: Uuid, deadline: i64 },
    RefreshUnknown,
    AuthorizationRequired,
    Disconnected,
}

/// Safe display/admission metadata. Never includes tokens or a provider URL.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub connection_ref: Uuid,
    pub label: String,
    pub registration: Registration,
    pub authorization_revision: u64,
    pub token_version: u64,
    pub enabled: bool,
    pub credential_state: CredentialState,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    connection: Connection,
    tokens: Option<Tokens>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    owner: String,
    host_id: String,
    connections: BTreeMap<Uuid, Record>,
}

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    owner: String,
}

pub struct Refresh {
    pub connection: Connection,
    pub tokens: Tokens,
    operation: Uuid,
    owner: String,
}

/// A short credential-generation lease held only until the execution fence.
/// Provider I/O must never retain it or prevent disconnect/rotation.
pub struct ExecutionLease {
    _lock: std::fs::File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshFailure {
    /// The exchange was not invoked; no token could have been consumed.
    NotSent,
    /// The request may have reached OpenAI, including a lost response.
    Uncertain,
    /// An authoritative token error made this grant unusable.
    Rejected,
}

impl std::fmt::Debug for Refresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Refresh").finish_non_exhaustive()
    }
}

impl Store {
    pub fn new(scope: &crate::LocalModelScope) -> Self {
        Self {
            path: scope.root().join("chatgpt.json"),
            owner: scope.identity().to_owned(),
        }
    }

    fn load(&self) -> Result<State, Error> {
        let bytes = match read_private_file(&self.path) {
            Ok(bytes) => bytes,
            Err(crate::LocalModelConfigError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(State {
                    version: VERSION,
                    owner: self.owner.clone(),
                    host_id: format!("urn:uuid:{}", Uuid::new_v4()),
                    connections: BTreeMap::new(),
                });
            }
            Err(_) => return Err(Error::Storage),
        };
        let state: State = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidStorage)?;
        if state.version != VERSION
            || state.owner != self.owner
            || state.connections.len() > MAX_CONNECTIONS
            || state
                .host_id
                .strip_prefix("urn:uuid:")
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_none()
        {
            return Err(Error::InvalidStorage);
        }
        for (id, record) in &state.connections {
            if *id != record.connection.connection_ref
                || record.connection.authorization_revision == 0
                || record.connection.token_version == 0
                || record.connection.label.is_empty()
                || record.connection.label.len() > 128
                || record.connection.label.chars().any(char::is_control)
            {
                return Err(Error::InvalidStorage);
            }
            record.connection.registration.validate()?;
            if record.connection.enabled
                && record.connection.credential_state == CredentialState::PlanDisabled
            {
                return Err(Error::InvalidStorage);
            }
            match (&record.connection.credential_state, &record.tokens) {
                (CredentialState::Disconnected | CredentialState::AuthorizationRequired, None) => {}
                (
                    CredentialState::Disconnected | CredentialState::AuthorizationRequired,
                    Some(_),
                )
                | (_, None) => return Err(Error::InvalidStorage),
                (_, Some(tokens)) => tokens.validate()?,
            }
        }
        Ok(state)
    }

    /// Every state transition is serialized by the same OS lock. Error paths
    /// may still persist a terminal RefreshUnknown transition; never undo it.
    fn mutate<T>(&self, update: impl FnOnce(&mut State) -> Result<T, Error>) -> Result<T, Error> {
        ensure_private_directory(self.path.parent().ok_or(Error::Storage)?)
            .map_err(|_| Error::Storage)?;
        let lock = open_lock(&self.path.with_extension("lock")).map_err(|_| Error::Storage)?;
        lock.lock_exclusive().map_err(|_| Error::Storage)?;
        let mut state = self.load()?;
        let result = update(&mut state);
        let bytes = serde_json::to_vec(&state).map_err(|_| Error::Storage)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(Error::Capacity);
        }
        write_atomic_private(&self.path, &bytes).map_err(|_| Error::Storage)?;
        result
    }

    /// Persist the installation identity before opening the provider browser.
    pub fn host_id(&self) -> Result<String, Error> {
        self.mutate(|state| Ok(state.host_id.clone()))
    }

    pub fn list(&self) -> Result<Vec<Connection>, Error> {
        Ok(self
            .load()?
            .connections
            .into_values()
            .map(|record| record.connection)
            .collect())
    }

    /// Store only a fully verified grant. Reauthorization is a CAS and creates
    /// a new authorization revision, while ordinary token refresh does not.
    pub fn authorize(
        &self,
        id: Uuid,
        expected_revision: Option<u64>,
        label: String,
        registration: Registration,
        tokens: Tokens,
        plan_authorized: bool,
    ) -> Result<Connection, Error> {
        registration.validate()?;
        tokens.validate()?;
        if label.is_empty() || label.len() > 128 || label.chars().any(char::is_control) {
            return Err(Error::InvalidCredentials);
        }
        self.mutate(|state| {
            let revision = match (state.connections.get(&id), expected_revision) {
                (None, None) if state.connections.len() < MAX_CONNECTIONS => 1,
                (None, None) => return Err(Error::Capacity),
                (Some(record), Some(expected))
                    if record.connection.authorization_revision == expected
                        && record.connection.registration == registration =>
                {
                    expected.checked_add(1).ok_or(Error::Changed)?
                }
                _ => return Err(Error::Changed),
            };
            let connection = Connection {
                connection_ref: id,
                label,
                registration,
                authorization_revision: revision,
                token_version: 1,
                enabled: plan_authorized,
                credential_state: if plan_authorized {
                    CredentialState::Ready
                } else {
                    CredentialState::PlanDisabled
                },
            };
            state.connections.insert(
                id,
                Record {
                    connection: connection.clone(),
                    tokens: Some(tokens),
                },
            );
            Ok(connection)
        })
    }

    pub fn token_snapshot(&self, id: Uuid) -> Result<(Connection, Tokens), Error> {
        let record = self
            .load()?
            .connections
            .remove(&id)
            .ok_or(Error::NotFound)?;
        match record.connection.credential_state {
            CredentialState::Ready => {}
            CredentialState::Refreshing { .. } => return Err(Error::RefreshPending),
            CredentialState::RefreshUnknown => return Err(Error::RefreshUnknown),
            _ => return Err(Error::Reauthorize),
        }
        if !record.connection.enabled {
            return Err(Error::Inactive);
        }
        Ok((record.connection, record.tokens.ok_or(Error::Reauthorize)?))
    }

    pub fn execution_lease(
        &self,
        id: Uuid,
        authorization_revision: u64,
        token_version: u64,
    ) -> Result<ExecutionLease, Error> {
        let lock = open_lock(&self.path.with_extension("lock")).map_err(|_| Error::Storage)?;
        FileExt::lock_shared(&lock).map_err(|_| Error::Storage)?;
        let record = self
            .load()?
            .connections
            .remove(&id)
            .ok_or(Error::NotFound)?;
        if !record.connection.enabled
            || record.connection.authorization_revision != authorization_revision
            || record.connection.token_version != token_version
            || record.connection.credential_state != CredentialState::Ready
        {
            return Err(Error::Changed);
        }
        Ok(ExecutionLease { _lock: lock })
    }

    pub fn begin_refresh(
        &self,
        id: Uuid,
        authorization_revision: u64,
        version: u64,
        now: i64,
    ) -> Result<Refresh, Error> {
        self.mutate(|state| {
            let record = state.connections.get_mut(&id).ok_or(Error::NotFound)?;
            if record.connection.authorization_revision != authorization_revision {
                return Err(Error::Changed);
            }
            match record.connection.credential_state {
                CredentialState::Refreshing { deadline, .. } if deadline <= now => {
                    record.connection.credential_state = CredentialState::RefreshUnknown;
                    return Err(Error::RefreshUnknown);
                }
                CredentialState::Refreshing { .. } => return Err(Error::RefreshPending),
                CredentialState::RefreshUnknown => return Err(Error::RefreshUnknown),
                CredentialState::Ready => {}
                _ => return Err(Error::Reauthorize),
            }
            if !record.connection.enabled {
                return Err(Error::Inactive);
            }
            if record.connection.token_version != version {
                return Err(Error::Changed);
            }
            let tokens = record.tokens.clone().ok_or(Error::Reauthorize)?;
            let operation = Uuid::new_v4();
            let connection = record.connection.clone();
            record.connection.credential_state = CredentialState::Refreshing {
                operation,
                deadline: now
                    .checked_add(REFRESH_LEASE_SECONDS)
                    .ok_or(Error::Changed)?,
            };
            Ok(Refresh {
                connection,
                tokens,
                operation,
                owner: self.owner.clone(),
            })
        })
    }

    fn refresh_record<'a>(
        &self,
        state: &'a mut State,
        refresh: &Refresh,
    ) -> Result<&'a mut Record, Error> {
        if refresh.owner != self.owner {
            return Err(Error::Changed);
        }
        let record = state
            .connections
            .get_mut(&refresh.connection.connection_ref)
            .ok_or(Error::Changed)?;
        if record.connection.authorization_revision != refresh.connection.authorization_revision
            || record.connection.token_version != refresh.connection.token_version
            || !matches!(record.connection.credential_state, CredentialState::Refreshing { operation, .. } if operation == refresh.operation)
        {
            return Err(Error::Changed);
        }
        Ok(record)
    }

    pub fn finish_refresh(&self, refresh: &Refresh, tokens: Tokens) -> Result<Connection, Error> {
        tokens.validate()?;
        self.mutate(|state| {
            let record = self.refresh_record(state, refresh)?;
            record.connection.token_version = record
                .connection
                .token_version
                .checked_add(1)
                .ok_or(Error::Changed)?;
            record.connection.credential_state = CredentialState::Ready;
            record.tokens = Some(tokens);
            // Do not re-enable a connection suspended by MOI logout.
            Ok(record.connection.clone())
        })
    }

    pub fn fail_refresh(&self, refresh: &Refresh, failure: RefreshFailure) -> Result<(), Error> {
        self.mutate(|state| {
            let record = self.refresh_record(state, refresh)?;
            record.connection.credential_state = match failure {
                RefreshFailure::NotSent => CredentialState::Ready,
                RefreshFailure::Uncertain => CredentialState::RefreshUnknown,
                RefreshFailure::Rejected => {
                    record.tokens = None;
                    CredentialState::AuthorizationRequired
                }
            };
            Ok(())
        })
    }

    pub fn suspend(&self) -> Result<(), Error> {
        self.mutate(|state| {
            for record in state.connections.values_mut() {
                record.connection.enabled = false;
            }
            Ok(())
        })
    }

    pub fn enable(&self, id: Uuid, revision: u64) -> Result<Connection, Error> {
        self.mutate(|state| {
            let record = state.connections.get_mut(&id).ok_or(Error::NotFound)?;
            if record.connection.authorization_revision != revision {
                return Err(Error::Changed);
            }
            if record.connection.credential_state != CredentialState::Ready {
                return Err(Error::Reauthorize);
            }
            record.connection.enabled = true;
            Ok(record.connection.clone())
        })
    }

    /// Clear tokens before remote revocation. A failed/unknown revoke cannot
    /// revive this connection, and a late refresh cannot recreate its tokens.
    pub fn disconnect(&self, id: Uuid) -> Result<(Connection, Option<Tokens>), Error> {
        self.mutate(|state| {
            let record = state.connections.get_mut(&id).ok_or(Error::NotFound)?;
            record.connection.authorization_revision = record
                .connection
                .authorization_revision
                .checked_add(1)
                .ok_or(Error::Changed)?;
            record.connection.enabled = false;
            record.connection.credential_state = CredentialState::Disconnected;
            Ok((record.connection.clone(), record.tokens.take()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    struct Fixture {
        _directory: tempfile::TempDir,
        store: Store,
        id: Uuid,
    }

    fn tokens(suffix: &str) -> Tokens {
        Tokens::new(
            format!("synthetic-access-{suffix}"),
            format!("synthetic-refresh-{suffix}"),
            3600,
        )
        .unwrap()
    }

    fn registration() -> Registration {
        Registration {
            client_id: "synthetic-issued-client".into(),
            subject: "synthetic-openai-user".into(),
            host_id: Uuid::new_v4().urn().to_string(),
        }
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let store = Store {
                path: directory.path().join("connections.json"),
                owner: "synthetic-platform-owner".into(),
            };
            let id = Uuid::new_v4();
            store
                .authorize(
                    id,
                    None,
                    "Fixture".into(),
                    registration(),
                    tokens("first"),
                    true,
                )
                .unwrap();
            Self {
                _directory: directory,
                store,
                id,
            }
        }
    }

    #[test]
    fn execution_fence_serializes_disconnect_and_never_accepts_old_token_versions() {
        let fixture = Fixture::new();
        let lease = fixture.store.execution_lease(fixture.id, 1, 1).unwrap();
        let writer = open_lock(&fixture.store.path.with_extension("lock")).unwrap();
        assert_eq!(
            FileExt::try_lock_exclusive(&writer).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(lease);
        FileExt::try_lock_exclusive(&writer).unwrap();
        drop(writer);
        let refresh = fixture.store.begin_refresh(fixture.id, 1, 1, 100).unwrap();
        assert!(fixture.store.execution_lease(fixture.id, 1, 1).is_err());
        fixture
            .store
            .finish_refresh(&refresh, tokens("second"))
            .unwrap();
        assert!(fixture.store.execution_lease(fixture.id, 1, 1).is_err());
        assert!(fixture.store.execution_lease(fixture.id, 1, 2).is_ok());
        fixture.store.disconnect(fixture.id).unwrap();
        assert!(fixture.store.execution_lease(fixture.id, 1, 2).is_err());
    }

    #[test]
    fn concurrent_refresh_claims_have_exactly_one_writer() {
        let fixture = Fixture::new();
        let barrier = Arc::new(Barrier::new(12));
        let workers: Vec<_> = (0..12)
            .map(|_| {
                let store = fixture.store.clone();
                let barrier = barrier.clone();
                let id = fixture.id;
                std::thread::spawn(move || {
                    barrier.wait();
                    store.begin_refresh(id, 1, 1, 100)
                })
            })
            .collect();
        let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|r| matches!(r, Err(Error::RefreshPending)))
                .count(),
            11
        );
    }

    #[test]
    fn rotation_after_platform_logout_saves_tokens_without_reenabling() {
        let fixture = Fixture::new();
        let refresh = fixture.store.begin_refresh(fixture.id, 1, 1, 100).unwrap();
        fixture.store.suspend().unwrap();
        let saved = fixture
            .store
            .finish_refresh(&refresh, tokens("replacement"))
            .unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.token_version, 2);
        assert_eq!(saved.authorization_revision, 1);
        assert!(matches!(
            fixture.store.token_snapshot(fixture.id),
            Err(Error::Inactive)
        ));
        fixture.store.enable(fixture.id, 1).unwrap();
        let (_, actual) = fixture.store.token_snapshot(fixture.id).unwrap();
        assert_eq!(actual.refresh_token(), "synthetic-refresh-replacement");
    }

    #[test]
    fn disconnect_fences_late_refresh_and_does_not_recreate_credentials() {
        let fixture = Fixture::new();
        let refresh = fixture.store.begin_refresh(fixture.id, 1, 1, 100).unwrap();
        let (_, revoked) = fixture.store.disconnect(fixture.id).unwrap();
        assert!(revoked.is_some());
        assert!(matches!(
            fixture.store.finish_refresh(&refresh, tokens("late")),
            Err(Error::Changed)
        ));
        let state = fixture.store.load().unwrap();
        assert!(state.connections[&fixture.id].tokens.is_none());
        assert_eq!(
            state.connections[&fixture.id].connection.credential_state,
            CredentialState::Disconnected
        );
    }

    #[test]
    fn refresh_unknown_is_sticky_across_process_restart_and_manual_enable() {
        let fixture = Fixture::new();
        let refresh = fixture.store.begin_refresh(fixture.id, 1, 1, 100).unwrap();
        fixture
            .store
            .fail_refresh(&refresh, RefreshFailure::Uncertain)
            .unwrap();
        let reopened = fixture.store.clone();
        assert!(matches!(
            reopened.begin_refresh(fixture.id, 1, 1, 1000),
            Err(Error::RefreshUnknown)
        ));
        assert!(matches!(
            reopened.enable(fixture.id, 1),
            Err(Error::Reauthorize)
        ));
        assert!(matches!(
            reopened.token_snapshot(fixture.id),
            Err(Error::RefreshUnknown)
        ));
        assert!(
            reopened.load().unwrap().connections[&fixture.id]
                .tokens
                .is_some()
        );
    }

    #[test]
    fn abandoned_refresh_cannot_consume_old_token_again() {
        let fixture = Fixture::new();
        fixture.store.begin_refresh(fixture.id, 1, 1, 100).unwrap();
        assert!(matches!(
            fixture.store.begin_refresh(fixture.id, 1, 1, 131),
            Err(Error::RefreshUnknown)
        ));
        // A later clock adjustment must not turn an expired intent back into a claim.
        assert!(matches!(
            fixture.store.begin_refresh(fixture.id, 1, 1, 100),
            Err(Error::RefreshUnknown)
        ));
    }

    #[test]
    fn authoritative_rejection_clears_unusable_tokens_but_not_registration() {
        let fixture = Fixture::new();
        let refresh = fixture.store.begin_refresh(fixture.id, 1, 1, 100).unwrap();
        fixture
            .store
            .fail_refresh(&refresh, RefreshFailure::Rejected)
            .unwrap();
        assert!(matches!(
            fixture.store.token_snapshot(fixture.id),
            Err(Error::Reauthorize)
        ));
        let record = fixture
            .store
            .load()
            .unwrap()
            .connections
            .remove(&fixture.id)
            .unwrap();
        assert!(record.tokens.is_none());
        assert_eq!(
            record.connection.registration.client_id,
            "synthetic-issued-client"
        );
    }

    #[test]
    fn preparation_failure_before_exchange_can_be_retried() {
        let fixture = Fixture::new();
        let refresh = fixture.store.begin_refresh(fixture.id, 1, 1, 100).unwrap();
        fixture
            .store
            .fail_refresh(&refresh, RefreshFailure::NotSent)
            .unwrap();
        assert!(fixture.store.begin_refresh(fixture.id, 1, 1, 101).is_ok());
    }

    #[test]
    fn reauthorization_invalidates_old_snapshot_even_when_token_version_resets() {
        let fixture = Fixture::new();
        fixture
            .store
            .authorize(
                fixture.id,
                Some(1),
                "New grant".into(),
                fixture.store.list().unwrap()[0].registration.clone(),
                tokens("new-grant"),
                true,
            )
            .unwrap();
        assert!(matches!(
            fixture.store.begin_refresh(fixture.id, 1, 1, 100),
            Err(Error::Changed)
        ));
        assert!(fixture.store.begin_refresh(fixture.id, 2, 1, 100).is_ok());
    }

    #[test]
    fn host_identity_is_persisted_before_authorization_and_survives_reopen() {
        let fixture = Fixture::new();
        let host = fixture.store.host_id().unwrap();
        assert!(host.starts_with("urn:uuid:"));
        assert_eq!(fixture.store.clone().host_id().unwrap(), host);
        fixture.store.suspend().unwrap();
        assert_eq!(fixture.store.host_id().unwrap(), host);
    }

    #[test]
    fn reauthorization_cannot_overwrite_another_provider_registration() {
        let fixture = Fixture::new();
        let mut other = fixture.store.list().unwrap()[0].registration.clone();
        other.subject = "other-provider-account".into();
        assert!(matches!(
            fixture.store.authorize(
                fixture.id,
                Some(1),
                "Other".into(),
                other,
                tokens("other"),
                true
            ),
            Err(Error::Changed)
        ));
        assert_eq!(
            fixture
                .store
                .token_snapshot(fixture.id)
                .unwrap()
                .1
                .access_token(),
            "synthetic-access-first"
        );
    }

    #[test]
    fn declined_plan_consent_retains_registration_without_enabling_inference() {
        let fixture = Fixture::new();
        let registration = fixture.store.list().unwrap()[0].registration.clone();
        let connection = fixture
            .store
            .authorize(
                fixture.id,
                Some(1),
                "Plan disabled".into(),
                registration,
                tokens("disabled"),
                false,
            )
            .unwrap();
        assert_eq!(connection.credential_state, CredentialState::PlanDisabled);
        assert!(!connection.enabled);
        assert!(matches!(
            fixture.store.enable(fixture.id, 2),
            Err(Error::Reauthorize)
        ));
        assert!(fixture.store.token_snapshot(fixture.id).is_err());
        assert!(
            fixture.store.load().unwrap().connections[&fixture.id]
                .tokens
                .is_some()
        );
    }

    #[test]
    fn oversized_replacement_does_not_destroy_readable_credential_state() {
        let fixture = Fixture::new();
        for _ in 0..8 {
            let id = Uuid::new_v4();
            let huge = Tokens::new("a".repeat(65_536), "r".repeat(65_536), 3600).unwrap();
            let result =
                fixture
                    .store
                    .authorize(id, None, "Large".into(), registration(), huge, true);
            if let Err(error) = result {
                assert_eq!(error, Error::Capacity);
            }
        }
        assert!(fixture.store.path.metadata().unwrap().len() <= MAX_STATE_BYTES as u64);
        assert!(fixture.store.token_snapshot(fixture.id).is_ok());
    }

    #[test]
    fn safe_metadata_and_debug_do_not_expose_tokens() {
        let fixture = Fixture::new();
        let views = serde_json::to_string(&fixture.store.list().unwrap()).unwrap();
        assert!(!views.contains("synthetic-access"));
        assert!(!views.contains("synthetic-refresh"));
        let refresh = fixture.store.begin_refresh(fixture.id, 1, 1, 100).unwrap();
        assert!(!format!("{refresh:?} {:?}", refresh.tokens).contains("synthetic"));
    }

    #[test]
    fn another_owner_and_corrupt_storage_fail_closed() {
        let fixture = Fixture::new();
        let other = Store {
            path: fixture.store.path.clone(),
            owner: "other-platform-user".into(),
        };
        assert!(matches!(other.list(), Err(Error::InvalidStorage)));
        std::fs::write(&fixture.store.path, b"{broken").unwrap();
        assert!(matches!(fixture.store.list(), Err(Error::InvalidStorage)));
    }

    #[test]
    #[cfg(unix)]
    fn token_file_is_private_and_symlink_reads_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        assert_eq!(
            std::fs::metadata(&fixture.store.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let target = fixture.store.path.with_extension("target");
        std::fs::rename(&fixture.store.path, &target).unwrap();
        symlink(&target, &fixture.store.path).unwrap();
        assert!(matches!(fixture.store.list(), Err(Error::Storage)));
    }
}
