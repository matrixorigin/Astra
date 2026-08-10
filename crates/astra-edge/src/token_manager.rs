//! Single owner of the edge token state machine.
//!
//! Four things used to race each other across scattered CAS patches and
//! guards: the in-memory token, the persisted token file, the one-shot
//! startup fallback candidate, and in-flight renewal responses. Review rounds
//! 8–11 each found another interleaving. This module ends that class of bug
//! structurally: **every transition happens under ONE async mutex**, so the
//! invariants below hold by construction instead of by per-call-site
//! vigilance.
//!
//! Invariants (all enforced inside the lock):
//!   1. `current` only changes via: startup selection, a renewal whose
//!      request snapshot still equals `current` (stale responses are
//!      discarded), or a one-shot fallback swap after a permanent auth
//!      failure.
//!   2. The token FILE never regresses **within this process**: renewal and
//!      retry writes carry `current`; heal-writes (AuthOk) carry the proven
//!      connection snapshot, gated by the generation rule
//!      ([`should_persist_proven`]) so a stale snapshot can never overwrite a
//!      strictly newer persisted token.
//!   3. A failed file write marks the state dirty; the dirty flag can never
//!      be lost to a concurrent transition because clearing it happens in the
//!      same critical section that re-reads `current` and writes the file.
//!   4. Non-MOI explicit credentials never mix with the MOI token file
//!      (enforced at startup selection in `resolve_config` and by the
//!      generation rule).
//!
//! Known limitation (documented, accepted): the guarantees are
//! **single-process**. Two processes sharing a workspace (rolling restart
//! overlap) cannot regress each other's atomic writes ([`write_token_atomic`]
//! uses unique temp names + rename), but the read-check-write in
//! [`TokenManager::mark_proven`] is not cross-process atomic, so a
//! transiently stale write can win the rename race; the next successful
//! authentication heals it. A cross-process flock is the upgrade path if
//! overlapping runs become a supported topology.
//!
//! File IO runs synchronously inside the async mutex — a deliberate tradeoff:
//! writes are tiny and rare, the critical section has no nested locks and no
//! external awaits (verified: no deadlock), and it is exactly what makes the
//! read-check-write sequences atomic in-process. Revisit with spawn_blocking
//! only if slow volumes show up in practice.

use std::path::PathBuf;
use std::sync::Arc;

use crate::token_renewal::{
    read_valid_file_token, should_persist_proven, token_file_needs_permission_repair,
    write_token_atomic,
};

/// Result of applying a renewal response.
#[derive(Debug, PartialEq, Eq)]
pub enum RenewApply {
    /// Installed as the current token and persisted.
    Applied,
    /// Installed as the current token, but the file write failed — the state
    /// is dirty and the caller should schedule fast persistence retries.
    AppliedUnpersisted,
    /// The shared token changed while the renewal was in flight (fallback
    /// swap or another writer). The response was discarded; renew again from
    /// the new current token.
    Discarded,
}

struct TokenState {
    current: String,
    /// One-shot alternate startup credential (same identity domain). Consumed
    /// by the first permanent auth failure.
    fallback: Option<String>,
    /// The token file is behind `current` (a write failed). Persistence
    /// retries clear this in the same critical section that writes the file.
    dirty: bool,
}

/// Single owner of the edge token: memory value, token file, fallback
/// candidate and persistence debt all live behind one lock.
pub struct TokenManager {
    state: tokio::sync::Mutex<TokenState>,
    token_file: PathBuf,
    /// Wakes the persistence-retry consumer when the state becomes dirty.
    persist_notify: tokio::sync::Notify,
}

impl std::fmt::Debug for TokenManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenManager")
            .field("token_file", &self.token_file)
            .finish()
    }
}

impl TokenManager {
    pub fn new(initial: String, fallback: Option<String>, token_file: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            state: tokio::sync::Mutex::new(TokenState {
                current: initial,
                fallback,
                dirty: false,
            }),
            token_file,
            persist_notify: tokio::sync::Notify::new(),
        })
    }

    /// The token to use for a new connection attempt / renewal request.
    pub async fn snapshot(&self) -> String {
        self.state.lock().await.current.clone()
    }

    /// A connection authenticated successfully with `proven`. Heal the token
    /// file when the generation rule allows it (never regress past a strictly
    /// newer persisted token; ties go to the proven token; non-MOI tokens
    /// never touch the file). When the file already contains exactly `proven`,
    /// rewrite it if needed to enforce private credential-file permissions.
    pub async fn mark_proven(&self, proven: &str) {
        let mut state = self.state.lock().await;
        // Expired file tokens are treated as absent: a dead token must not win
        // the generation comparison against a token that just proved itself.
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let existing = read_valid_file_token(&self.token_file, now_unix);
        let repair_permissions = existing.as_deref() == Some(proven)
            && token_file_needs_permission_repair(&self.token_file);
        if !repair_permissions && !should_persist_proven(existing.as_deref(), proven) {
            return;
        }
        match write_token_atomic(&self.token_file, proven) {
            Ok(()) => {
                // The file now holds `proven`; when that IS the current token,
                // any outstanding debt has just been settled by this write.
                if state.current == proven {
                    state.dirty = false;
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra.edge",
                    path = %self.token_file.display(),
                    error = %error,
                    "failed to persist proven-good edge token — persistence retry scheduled"
                );
                // Only mark dirty when the proven token IS the current one:
                // retries always write `current`, and overwriting the file
                // with a stale proven snapshot via the retry path would
                // regress it.
                if state.current == proven {
                    state.dirty = true;
                    self.persist_notify.notify_one();
                }
            }
        }
    }

    /// Apply a renewal response that was requested with `requested_with`.
    /// Discarded (without side effects) when the current token changed while
    /// the request was in flight — the concurrent switch wins.
    pub async fn apply_renewed(&self, requested_with: &str, new_token: String) -> RenewApply {
        let mut state = self.state.lock().await;
        if state.current != requested_with {
            return RenewApply::Discarded;
        }
        state.current = new_token;
        if let Err(error) = write_token_atomic(&self.token_file, &state.current) {
            tracing::warn!(
                target: "astra.edge",
                path = %self.token_file.display(),
                error = %error,
                "failed to persist renewed edge token — persistence retry scheduled"
            );
            state.dirty = true;
            self.persist_notify.notify_one();
            return RenewApply::AppliedUnpersisted;
        }
        state.dirty = false;
        RenewApply::Applied
    }

    /// Swap to the one-shot fallback credential after a permanent auth
    /// failure. Returns false when no fallback is available (caller gives up).
    /// The fallback is not persisted here: it must prove itself first
    /// (`mark_proven` on its first successful authentication).
    pub async fn swap_to_fallback(&self) -> bool {
        let mut state = self.state.lock().await;
        match state.fallback.take() {
            Some(fallback) => {
                state.current = fallback;
                // The file intentionally keeps the previous credential until
                // the fallback proves itself. Dropping any outstanding debt is
                // deliberate: retrying would persist the UNPROVEN fallback,
                // and the failed previous current is not worth writing either.
                state.dirty = false;
                // Wake the renewal loop: the fallback is by definition the
                // older startup candidate and may be close to expiry — it
                // must not wait out the 6h check interval (M2, round 12).
                self.persist_notify.notify_one();
                true
            }
            None => false,
        }
    }

    /// Attempt to clear persistence debt: re-reads `current` and writes it in
    /// the SAME critical section, so a token switched in after a previous
    /// failed write can never be skipped. Returns true when the state is
    /// clean afterwards.
    pub async fn retry_persist(&self) -> bool {
        let mut state = self.state.lock().await;
        if !state.dirty {
            return true;
        }
        match write_token_atomic(&self.token_file, &state.current) {
            Ok(()) => {
                state.dirty = false;
                tracing::info!(
                    target: "astra.edge",
                    path = %self.token_file.display(),
                    "edge token persistence recovered"
                );
                true
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra.edge",
                    path = %self.token_file.display(),
                    error = %error,
                    "retry of edge token persistence failed"
                );
                false
            }
        }
    }

    /// Await a persistence-debt wakeup (used by the retry consumers).
    pub async fn persist_debt_notified(&self) {
        self.persist_notify.notified().await;
    }

    /// Whether the token file is currently behind memory.
    pub async fn persist_pending(&self) -> bool {
        self.state.lock().await.dirty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic tokens use a far-future epoch base so the real-clock expiry
    /// check in mark_proven never treats them as dead.
    const TEST_EPOCH_BASE: i64 = 4_000_000_000;

    fn make_moi_token(iat: i64, exp: i64, jti: &str) -> String {
        use base64::Engine as _;
        // sub/workspace_id are required for a token the server (and now
        // read_valid_file_token) would accept. All tokens here share one
        // identity, so same-identity generation ordering still applies.
        let claims = serde_json::json!({
            "iat": TEST_EPOCH_BASE + iat,
            "exp": TEST_EPOCH_BASE + exp,
            "jti": jti,
            "sub": "user-1",
            "workspace_id": "workspace-1",
        });
        let encoded =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        format!("moi-user-token-v1.{encoded}.sig")
    }

    fn temp_token_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "astra-edge-token-manager-{tag}-{}-{:x}",
                std::process::id(),
                fastrand::u64(..)
            ))
            .join("token")
    }

    #[tokio::test]
    async fn stale_renewal_response_is_discarded() {
        let a = make_moi_token(100, 200, "A");
        let b = make_moi_token(100, 200, "B");
        let c = make_moi_token(101, 201, "C");
        let manager = TokenManager::new(a.clone(), Some(b.clone()), temp_token_path("stale"));

        // Renewal was requested with A; a fallback swap to B raced in.
        assert!(manager.swap_to_fallback().await);
        assert_eq!(manager.snapshot().await, b);
        assert_eq!(manager.apply_renewed(&a, c).await, RenewApply::Discarded);
        assert_eq!(
            manager.snapshot().await,
            b,
            "the concurrent switch must win"
        );
    }

    #[tokio::test]
    async fn renewal_applies_and_persists_when_snapshot_matches() {
        let a = make_moi_token(100, 200, "A");
        let c = make_moi_token(101, 201, "C");
        let path = temp_token_path("apply");
        let manager = TokenManager::new(a.clone(), None, path.clone());

        assert_eq!(
            manager.apply_renewed(&a, c.clone()).await,
            RenewApply::Applied
        );
        assert_eq!(manager.snapshot().await, c);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), c);
        assert!(!manager.persist_pending().await);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn dirty_state_persists_the_latest_current_not_the_failed_one() {
        // The round-10 lost-update scenario, now structurally impossible:
        // a failed write marks dirty; before the retry runs, the current
        // token changes again; the retry writes the LATEST current because
        // read + write + clear share one critical section.
        let a = make_moi_token(100, 200, "A");
        let c = make_moi_token(101, 201, "C");
        let d = make_moi_token(102, 202, "D");
        let path = temp_token_path("dirty");
        let manager = TokenManager::new(a.clone(), None, path.clone());

        // Force a failed write by making the target's PARENT a file.
        let parent = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
        std::fs::write(&parent, "not a dir").unwrap();
        assert_eq!(
            manager.apply_renewed(&a, c.clone()).await,
            RenewApply::AppliedUnpersisted
        );
        assert!(manager.persist_pending().await);

        // Memory moves on to D while the debt is outstanding.
        assert_eq!(
            manager.apply_renewed(&c, d.clone()).await,
            RenewApply::AppliedUnpersisted
        );

        // Unblock the filesystem; the retry must write D (latest), not C.
        std::fs::remove_file(&parent).unwrap();
        assert!(manager.retry_persist().await);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), d);
        assert!(!manager.persist_pending().await);
        let _ = std::fs::remove_dir_all(parent);
    }

    #[tokio::test]
    async fn proven_heal_respects_generation_rule() {
        let old = make_moi_token(100, 200, "A");
        let newer = make_moi_token(101, 201, "B");
        let path = temp_token_path("proven");
        let manager = TokenManager::new(old.clone(), None, path.clone());

        // File holds a STRICTLY newer token (renewal forward progress): a
        // proven-but-older snapshot must not regress it.
        write_token_atomic(&path, &newer).unwrap();
        manager.mark_proven(&old).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);

        // Same-generation sibling (double-renew): the proven token wins.
        let sibling = make_moi_token(101, 201, "B2");
        manager.mark_proven(&sibling).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), sibling);

        // Non-MOI tokens never touch the file.
        manager.mark_proven("plain-astra-token").await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), sibling);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proven_token_repairs_insecure_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let token = make_moi_token(100, 200, "A");
        let path = temp_token_path("permissions");
        write_token_atomic(&path, &token).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let manager = TokenManager::new(token.clone(), None, path.clone());

        manager.mark_proven(&token).await;

        assert_eq!(std::fs::read_to_string(&path).unwrap(), token);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!manager.persist_pending().await);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn stale_proven_write_failure_must_not_arm_the_retry_path() {
        // proven != current: a heal-write failure must NOT set dirty, because
        // the retry path writes `current` — arming it from a stale proven
        // snapshot would be pointless (and confusing in logs).
        let a = make_moi_token(100, 200, "A");
        let c = make_moi_token(101, 201, "C");
        let path = temp_token_path("stale-proven");
        // Block writes: parent is a file.
        let parent = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
        std::fs::write(&parent, "not a dir").unwrap();
        let manager = TokenManager::new(c.clone(), None, path.clone());

        manager.mark_proven(&a).await; // proven A, current C, write fails
        assert!(
            !manager.persist_pending().await,
            "stale proven must not arm dirty"
        );
        let _ = std::fs::remove_file(&parent);
    }

    #[tokio::test]
    async fn fallback_swap_drops_outstanding_debt_by_design() {
        // An unpersisted current that just failed authentication is not worth
        // persisting, and the unproven fallback must not be persisted either:
        // the swap clears the debt and the file keeps the last persisted
        // value until the fallback proves itself via mark_proven.
        let a = make_moi_token(100, 200, "A");
        let b = make_moi_token(99, 199, "B");
        let c = make_moi_token(101, 201, "C");
        let path = temp_token_path("swap-debt");
        let parent = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
        std::fs::write(&parent, "not a dir").unwrap();
        let manager = TokenManager::new(a.clone(), Some(b.clone()), path.clone());

        assert_eq!(
            manager.apply_renewed(&a, c).await,
            RenewApply::AppliedUnpersisted
        );
        assert!(manager.persist_pending().await);
        assert!(manager.swap_to_fallback().await);
        assert!(
            !manager.persist_pending().await,
            "swap must drop the stale debt"
        );
        assert_eq!(manager.snapshot().await, b);
        let _ = std::fs::remove_file(&parent);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_mixed_operations_preserve_invariants() {
        // True-concurrency smoke test: renewal chains, proven heals and
        // persistence retries race across tasks. Afterwards the state must be
        // internally consistent: clean state ⇒ the file holds exactly the
        // current token.
        let base = make_moi_token(100, 200, "gen-0");
        let path = temp_token_path("stress");
        let manager = TokenManager::new(base.clone(), None, path.clone());

        let mut handles = Vec::new();
        // Renewal chain: each task renews from whatever snapshot it sees.
        // Generations are globally monotonic (like real renewals, where the
        // backend always signs now+TTL > the source token's iat/exp).
        let generation = Arc::new(std::sync::atomic::AtomicI64::new(101));
        for round in 0..4u32 {
            let manager = Arc::clone(&manager);
            let generation = Arc::clone(&generation);
            handles.push(tokio::spawn(async move {
                for step in 0..25u32 {
                    let snapshot = manager.snapshot().await;
                    let iat = generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let next = make_moi_token(iat, iat + 100, &format!("gen-{round}-{step}"));
                    let _ = manager.apply_renewed(&snapshot, next).await;
                }
            }));
        }
        // Heal writers racing with renewals.
        for _ in 0..2 {
            let manager = Arc::clone(&manager);
            handles.push(tokio::spawn(async move {
                for _ in 0..25 {
                    let snapshot = manager.snapshot().await;
                    manager.mark_proven(&snapshot).await;
                }
            }));
        }
        // Retry consumer.
        {
            let manager = Arc::clone(&manager);
            handles.push(tokio::spawn(async move {
                for _ in 0..50 {
                    let _ = manager.retry_persist().await;
                    tokio::task::yield_now().await;
                }
            }));
        }
        for handle in handles {
            handle.await.expect("task panicked");
        }
        // Converge and verify: clean ⇒ file == current.
        assert!(manager.retry_persist().await);
        let current = manager.snapshot().await;
        assert_eq!(
            std::fs::read_to_string(&path).expect("token file"),
            current,
            "clean state must mean the file holds the current token"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn fallback_is_one_shot_and_unpersisted_until_proven() {
        let a = make_moi_token(100, 200, "A");
        let b = make_moi_token(99, 199, "B");
        let path = temp_token_path("fallback");
        write_token_atomic(&path, &a).unwrap();
        let manager = TokenManager::new(a.clone(), Some(b.clone()), path.clone());

        assert!(manager.swap_to_fallback().await);
        assert_eq!(manager.snapshot().await, b);
        // File still holds A until B proves itself.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), a);
        // Second failure: no more candidates.
        assert!(!manager.swap_to_fallback().await);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
