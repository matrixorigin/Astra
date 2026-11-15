//! Bounded positive observations of ordinary JWT authentication. Credentials,
//! replay admission and fresh reauthentication never enter this cache.

use super::{AuthPrincipal, AuthPrincipalOrigin};
use lru::LruCache;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const TTL: Duration = Duration::from_secs(30);
const MAX_ENTRIES: usize = 16_384;
const MAX_SESSION_INVALIDATIONS: usize = 1_024;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 16 * 1024;
// Conservatively reserve for LRU nodes, hash buckets, session fences and
// retained allocations after churn, plus the temporary invalidation keys.
// This is an accounting budget, not a bound on allocator overhead or RSS.
const CONTAINER_BYTES: usize = MAX_ENTRIES * 4 * (std::mem::size_of::<([u8; 32], Entry)>() + 128)
    + MAX_SESSION_INVALIDATIONS * 4 * (std::mem::size_of::<([u8; 32], u64)>() + 16);

struct Entry {
    principal: AuthPrincipal,
    checked_at: Instant,
    expires_at_unix: i64,
    bytes: usize,
}

impl Entry {
    fn is_fresh(&self, now: Instant, now_unix: i64) -> bool {
        now.duration_since(self.checked_at) < TTL && now_unix < self.expires_at_unix
    }
}

struct State {
    generation: u64,
    global_invalidation: u64,
    session_invalidations: HashMap<[u8; 32], u64>,
    bytes: usize,
    entries: LruCache<[u8; 32], Entry>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            generation: 0,
            global_invalidation: 0,
            session_invalidations: HashMap::new(),
            bytes: 0,
            entries: LruCache::sparse(NonZeroUsize::new(MAX_ENTRIES).unwrap()),
        }
    }
}

#[derive(Default)]
pub(super) struct AuthenticationCache(Mutex<State>);

impl AuthenticationCache {
    pub(super) fn key(token: &str) -> [u8; 32] {
        Sha256::digest(token.as_bytes()).into()
    }

    fn session_key(user_id: &str, session_id: &str) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update((user_id.len() as u64).to_be_bytes());
        hash.update(user_id.as_bytes());
        hash.update(session_id.as_bytes());
        hash.finalize().into()
    }

    pub(super) fn lookup(&self, key: &[u8; 32]) -> (Option<AuthPrincipal>, u64) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let principal = match state.entries.get(key) {
            Some(entry) if entry.is_fresh(Instant::now(), chrono::Utc::now().timestamp()) => {
                Some(entry.principal.clone())
            }
            Some(_) => {
                if let Some(expired) = state.entries.pop(key) {
                    state.bytes -= expired.bytes;
                }
                None
            }
            None => None,
        };
        (principal, state.generation)
    }

    /// Retention is independent of the caller's successful authentication
    /// snapshot. A late fill cannot resurrect a logout/disconnect in the cache.
    pub(super) fn publish(
        &self,
        key: [u8; 32],
        generation: u64,
        checked_at: Instant,
        expires_at_unix: i64,
        principal: &AuthPrincipal,
    ) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let now_unix = chrono::Utc::now().timestamp();
        if state.generation != generation {
            let session_invalidated = principal.session_id.as_deref().is_some_and(|session| {
                state
                    .session_invalidations
                    .get(&Self::session_key(&principal.user.user_id, session))
                    .is_some_and(|last| *last > generation)
            });
            if generation < state.global_invalidation || session_invalidated {
                return false;
            }
        }
        if now_unix >= expires_at_unix || now.duration_since(checked_at) >= TTL {
            return false;
        }
        let bytes = principal_bytes(principal);
        if bytes > MAX_ENTRY_BYTES || CONTAINER_BYTES.saturating_add(bytes) > MAX_BYTES {
            return false;
        }
        if let Some(previous) = state.entries.pop(&key) {
            state.bytes -= previous.bytes;
        }
        while state.entries.len() >= MAX_ENTRIES
            || CONTAINER_BYTES
                .saturating_add(state.bytes)
                .saturating_add(bytes)
                > MAX_BYTES
        {
            let Some((_, evicted)) = state.entries.pop_lru() else {
                return false;
            };
            state.bytes -= evicted.bytes;
        }
        state.entries.push(
            key,
            Entry {
                principal: principal.clone(),
                checked_at,
                expires_at_unix,
                bytes,
            },
        );
        state.bytes += bytes;
        true
    }

    fn invalidate(&self, session: Option<(&str, &str)>) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation = state.generation.wrapping_add(1);
        if let Some((user_id, session_id)) = session {
            let key = Self::session_key(user_id, session_id);
            // Bound the fence metadata. On overflow, fence all older fills
            // before discarding their session-specific invalidations.
            if !state.session_invalidations.contains_key(&key)
                && state.session_invalidations.len() >= MAX_SESSION_INVALIDATIONS
            {
                state.global_invalidation = state.generation;
                state.session_invalidations.clear();
            }
            let generation = state.generation;
            state.session_invalidations.insert(key, generation);
            let keys: Vec<_> = state
                .entries
                .iter()
                .filter(|(_, entry)| {
                    entry.principal.user.user_id == user_id
                        && entry.principal.session_id.as_deref() == Some(session_id)
                })
                .map(|(key, _)| *key)
                .collect();
            for key in keys {
                if let Some(entry) = state.entries.pop(&key) {
                    state.bytes -= entry.bytes;
                }
            }
        } else {
            state.global_invalidation = state.generation;
            state.session_invalidations.clear();
            state.entries.clear();
            state.bytes = 0;
        }
    }

    pub(super) fn mutation(self: &Arc<Self>) -> InvalidateOnDrop {
        self.invalidate(None);
        InvalidateOnDrop {
            cache: Arc::clone(self),
            session: None,
        }
    }

    pub(super) fn session_mutation(
        self: &Arc<Self>,
        user_id: &str,
        session_id: &str,
    ) -> InvalidateOnDrop {
        self.invalidate(Some((user_id, session_id)));
        InvalidateOnDrop {
            cache: Arc::clone(self),
            session: Some((user_id.to_owned(), session_id.to_owned())),
        }
    }
}

pub(super) struct InvalidateOnDrop {
    cache: Arc<AuthenticationCache>,
    session: Option<(String, String)>,
}

impl Drop for InvalidateOnDrop {
    fn drop(&mut self) {
        // Includes errors, cancellation, and unknown commit acknowledgements.
        self.cache.invalidate(
            self.session
                .as_ref()
                .map(|(user, session)| (user.as_str(), session.as_str())),
        );
    }
}

fn principal_bytes(principal: &AuthPrincipal) -> usize {
    let user = &principal.user;
    let mut bytes = std::mem::size_of::<Entry>()
        + user.user_id.capacity()
        + user.username.capacity()
        + user.email.capacity()
        + user.display_name.as_ref().map_or(0, String::capacity)
        + principal.session_id.as_ref().map_or(0, String::capacity);
    if let Some(grant) = &principal.execution_continuation {
        bytes += grant.user_id.capacity() + grant.session_id.capacity();
    }
    if let AuthPrincipalOrigin::VerifiedProvider {
        provider_id,
        external_subject,
    } = &principal.origin
    {
        bytes += provider_id.capacity() + external_subject.capacity();
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthService, AuthUserRecord, DatabaseAuthService};
    use astra_core::{JwtSettings, MatrixOneSettings};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};

    fn principal() -> AuthPrincipal {
        AuthPrincipal::internal(AuthUserRecord {
            user_id: "owner".into(),
            username: "ordinary".into(),
            email: "ordinary@example.test".into(),
            display_name: None,
        })
    }

    #[test]
    fn freshness_is_absolute_and_invalidated_fills_cannot_repopulate() {
        let cache = Arc::new(AuthenticationCache::default());
        let key = AuthenticationCache::key("bearer-one");
        let (_, generation) = cache.lookup(&key);
        let expires = chrono::Utc::now().timestamp() + 60;
        let started = Instant::now();
        assert!(cache.publish(key, generation, started, expires, &principal()));
        for _ in 0..3 {
            assert!(cache.lookup(&key).0.is_some());
            assert_eq!(
                cache
                    .0
                    .lock()
                    .unwrap()
                    .entries
                    .peek(&key)
                    .unwrap()
                    .checked_at,
                started
            );
        }
        assert!(
            cache
                .lookup(&AuthenticationCache::key("other-session"))
                .0
                .is_none()
        );
        let mutation = cache.mutation();
        assert!(!cache.publish(key, generation, started, expires, &principal()));
        let (_, during_mutation) = cache.lookup(&key);
        drop(mutation);
        assert!(!cache.publish(key, during_mutation, started, expires, &principal()));
        let (_, generation) = cache.lookup(&key);
        assert!(!cache.publish(key, generation, started, expires - 120, &principal()));
        assert!(!cache.publish(key, generation, started - TTL, expires, &principal()));
        assert!(cache.lookup(&key).0.is_none());
    }

    #[test]
    fn ten_thousand_distinct_sessions_remain_warm() {
        let cache = AuthenticationCache::default();
        let now = Instant::now();
        let expires = chrono::Utc::now().timestamp() + 60;
        let mut value = principal();
        for index in 0..10_000 {
            value.user.user_id = format!("user-{index}");
            value.session_id = Some(format!("session-{index}"));
            let key = AuthenticationCache::key(&format!("token-{index}"));
            assert!(cache.publish(key, 0, now, expires, &value));
        }
        for index in 0..10_000 {
            let key = AuthenticationCache::key(&format!("token-{index}"));
            assert_eq!(
                cache.lookup(&key).0.unwrap().user.user_id,
                format!("user-{index}"),
                "a different user's authentication cannot displace the working set"
            );
        }
    }

    #[test]
    fn capacity_pressure_evicts_the_least_recent_observation() {
        let cache = AuthenticationCache::default();
        let now = Instant::now();
        let expires = chrono::Utc::now().timestamp() + 60;
        let value = principal();
        for index in 0..MAX_ENTRIES {
            let key = AuthenticationCache::key(&format!("token-{index}"));
            assert!(cache.publish(key, 0, now, expires, &value));
        }
        let first = AuthenticationCache::key("token-0");
        let second = AuthenticationCache::key("token-1");
        assert!(cache.lookup(&first).0.is_some());
        assert!(cache.publish(
            AuthenticationCache::key("overflow"),
            0,
            now,
            expires,
            &value
        ));
        assert!(cache.lookup(&first).0.is_some());
        assert!(cache.lookup(&second).0.is_none());

        // An LRU hit must never extend the original authentication deadline.
        cache
            .0
            .lock()
            .unwrap()
            .entries
            .peek_mut(&first)
            .unwrap()
            .checked_at = now - TTL;
        assert!(cache.lookup(&first).0.is_none());
    }

    #[test]
    fn session_invalidation_preserves_other_sessions_and_fences_inflight_fills() {
        let cache = Arc::new(AuthenticationCache::default());
        let first_key = AuthenticationCache::key("first");
        let first_extra_key = AuthenticationCache::key("first-extra-token");
        let second_key = AuthenticationCache::key("second");
        let other_key = AuthenticationCache::key("other");
        let other_fill = AuthenticationCache::key("other-cold-fill");
        let mut first = principal();
        first.session_id = Some("first-session".into());
        let mut second = first.clone();
        second.session_id = Some("second-session".into());
        let mut other = first.clone();
        other.user.user_id = "other-user".into();
        let now = Instant::now();
        let expires = chrono::Utc::now().timestamp() + 60;
        for (key, value) in [
            (first_key, &first),
            (first_extra_key, &first),
            (second_key, &second),
            (other_key, &other),
        ] {
            assert!(cache.publish(key, 0, now, expires, value));
        }
        let mut replacement = first.clone();
        replacement.user.display_name = Some("replacement".repeat(128));
        assert!(cache.publish(first_key, 0, now, expires, &replacement));
        assert_eq!(cache.lookup(&first_key).0, Some(replacement));
        {
            let state = cache.0.lock().unwrap();
            assert_eq!(
                state.bytes,
                state
                    .entries
                    .iter()
                    .map(|(_, entry)| entry.bytes)
                    .sum::<usize>()
            );
        }

        let mutation = cache.session_mutation("owner", "first-session");
        assert!(cache.lookup(&first_key).0.is_none());
        assert!(cache.lookup(&first_extra_key).0.is_none());
        assert_eq!(cache.lookup(&second_key).0, Some(second.clone()));
        assert_eq!(cache.lookup(&other_key).0, Some(other.clone()));
        assert!(!cache.publish(first_key, 0, now, expires, &first));
        let (_, during) = cache.lookup(&first_key);
        drop(mutation);
        assert!(!cache.publish(first_key, during, now, expires, &first));
        for _ in 0..100 {
            drop(cache.session_mutation("owner", "first-session"));
        }
        assert!(
            cache.publish(other_fill, 0, now, expires, &other),
            "a different session's in-flight database observation stays cacheable"
        );
        assert_eq!(cache.lookup(&second_key).0, Some(second));
        assert_eq!(cache.lookup(&other_key).0, Some(other));
    }

    #[test]
    fn session_fence_overflow_is_bounded_and_fails_closed() {
        let cache = Arc::new(AuthenticationCache::default());
        let now = Instant::now();
        let expires = chrono::Utc::now().timestamp() + 60;
        for index in 0..=MAX_SESSION_INVALIDATIONS {
            drop(cache.session_mutation("owner", &format!("session-{index}")));
        }
        let state = cache.0.lock().unwrap();
        assert!(state.session_invalidations.len() <= MAX_SESSION_INVALIDATIONS);
        assert!(state.global_invalidation > 0);
        drop(state);
        let key = AuthenticationCache::key("unrelated");
        assert!(!cache.publish(key, 0, now, expires, &principal()));
        let (_, current) = cache.lookup(&key);
        assert!(cache.publish(key, current, now, expires, &principal()));
    }

    #[test]
    fn cache_retention_is_bounded_and_oversized_principals_stay_local() {
        for content_bytes in [0, MAX_ENTRY_BYTES / 2] {
            let cache = Arc::new(AuthenticationCache::default());
            let expires = chrono::Utc::now().timestamp() + 300;
            let mut value = principal();
            value.user.display_name = Some("x".repeat(content_bytes));
            // Exercise actual remove/insert churn under both the entry and byte
            // limits, rather than checking the accounting formula against itself.
            for index in 0..2 * MAX_ENTRIES {
                assert!(cache.publish(
                    AuthenticationCache::key(&index.to_string()),
                    0,
                    Instant::now(),
                    expires,
                    &value
                ));
            }
            let state = cache.0.lock().unwrap();
            if content_bytes == 0 {
                assert_eq!(state.entries.len(), MAX_ENTRIES);
            } else {
                assert!(state.entries.len() < MAX_ENTRIES);
            }
            assert!(CONTAINER_BYTES + state.bytes <= MAX_BYTES);
            assert_eq!(
                state.bytes,
                state
                    .entries
                    .iter()
                    .map(|(_, entry)| entry.bytes)
                    .sum::<usize>()
            );
            drop(state);
            drop(cache.mutation());
            let state = cache.0.lock().unwrap();
            assert!(state.entries.is_empty());
            assert_eq!(state.bytes, 0);
        }
        let cache = AuthenticationCache::default();
        let mut large = principal();
        large.user.display_name = Some("x".repeat(MAX_ENTRY_BYTES));
        let key = AuthenticationCache::key("oversized");
        assert!(!cache.publish(
            key,
            0,
            Instant::now(),
            chrono::Utc::now().timestamp() + 60,
            &large
        ));
        assert!(cache.lookup(&key).0.is_none());
    }

    #[tokio::test]
    async fn warm_authentication_skips_database_but_always_checks_the_jwt() {
        let service = DatabaseAuthService::new(
            MatrixOneSettings::mock(),
            JwtSettings {
                secret_key: "local-cache-test-signing-key".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 5,
                refresh_token_expire_days: 1,
            },
        );
        let now = chrono::Utc::now().timestamp();
        let valid = serde_json::json!({"sub":"owner", "sid":"session-one", "origin":"internal",
            "type":"access", "iat":now, "exp":now+60});
        for case in ["valid", "signature", "type", "origin", "expiry", "session"] {
            let mut claims = valid.clone();
            match case {
                "type" => claims["type"] = "refresh".into(),
                "origin" => claims["origin"] = "unrecognized".into(),
                "expiry" => claims["exp"] = (now - 1).into(),
                "session" => {
                    claims.as_object_mut().unwrap().remove("sid");
                }
                _ => {}
            }
            let signing_key = if case == "signature" {
                "wrong-key"
            } else {
                &service.jwt.secret_key
            };
            let token = jsonwebtoken::encode(
                &jsonwebtoken::Header::default(),
                &claims,
                &jsonwebtoken::EncodingKey::from_secret(signing_key.as_bytes()),
            )
            .unwrap();
            let key = AuthenticationCache::key(&token);
            let (_, generation) = service.authentication_cache.lookup(&key);
            assert!(service.authentication_cache.publish(
                key,
                generation,
                Instant::now(),
                now + 60,
                &principal()
            ));
            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
            let result = service.clone().current_principal(&headers).await;
            if case == "valid" {
                assert_eq!(
                    result.expect("cache hit must work without a database"),
                    principal()
                );
            } else {
                assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED, "{case}");
            }
        }
    }
}
