use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use astra_core::SharedPool;

/// Hash a logical cache_key into a fixed-length (64-char) SHA-256 hex so the
/// `tool_exactly_once_results.dedup_key` column (part of the PRIMARY KEY)
/// never truncates. Two distinct cache_keys map to distinct hashes with
/// overwhelming probability; collisions at this scale are negligible and no
/// worse than the existing hash-of-args already used by `cache_key`.
/// The stored value is only a row uniquifier — semantic matching goes through
/// `key_json`, so hashing the uniquifier loses no information.
fn dedup_key_hash(cache_key: &str) -> String {
    let digest = Sha256::digest(cache_key.as_bytes());
    base16_encode_lower(&digest)
}

/// Lowercase hex without pulling in another crate.
fn base16_encode_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Wraps in-memory idempotency with optional DB persistence for crash recovery.
///
/// When `pool` is `Some`, successful results are persisted to the
/// `tool_exactly_once_results` table so a session that restarts after a crash
/// can reload previously-executed tool outcomes and avoid re-executing
/// side-effect tools.
pub(crate) struct ExactlyOnceState {
    pub(crate) in_memory: Mutex<astra_pipeline::exactly_once::ExactlyOnceExecutor>,
    pool: Option<SharedPool>,
    user_id: String,
    session_id: String,
}

// ── Construction ───────────────────────────────────────────────────────────

/// Create and optionally warm the exactly-once state from DB.
///
/// When `pool` is available, existing results for `session_id` are loaded from
/// `tool_exactly_once_results` into the in-memory cache. This is the crash-
/// recovery path: after a crash, the in-memory cache is empty but the DB holds
/// results from the pre-crash execution.
pub(crate) async fn enable_exactly_once(
    user_id: &str,
    session_id: &str,
    pool: Option<SharedPool>,
) -> ExactlyOnceState {
    let mut executor = astra_pipeline::exactly_once::ExactlyOnceExecutor::new();

    // Owner-bound server recovery must not hydrate from local event streams:
    // those artifacts are keyed by session_id only and carry no trustworthy
    // owner metadata. The DB table is the authoritative server cache because
    // every row is scoped by (user_id, session_id).
    if let Some(ref pool) = pool {
        if let Err(e) = warm_cache_from_db(&mut executor, user_id, session_id, pool).await {
            tracing::warn!(
                user_id = %user_id,
                session_id = %session_id,
                error = %e,
                "failed to warm exactly-once cache from DB; falling back to in-memory only"
            );
        }
    }

    ExactlyOnceState {
        in_memory: Mutex::new(executor),
        pool,
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
    }
}

async fn warm_cache_from_db(
    executor: &mut astra_pipeline::exactly_once::ExactlyOnceExecutor,
    user_id: &str,
    session_id: &str,
    pool: &SharedPool,
) -> Result<(), sqlx::Error> {
    use sqlx::Row;

    let rows = sqlx::query(
        "SELECT dedup_key, key_json, result_json FROM tool_exactly_once_results \
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_all(pool.get())
    .await?;

    let count = rows.len();
    for row in rows {
        let _dedup_key: String = row.try_get("dedup_key")?;
        let key_json: String = row.try_get("key_json")?;
        let result_json: String = row.try_get("result_json")?;
        let key: astra_pipeline::step_protocol::IdempotencyKey =
            match serde_json::from_str(&key_json) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        dedup_key = %_dedup_key,
                        "failed to deserialize exactly-once key from DB; skipping entry"
                    );
                    continue;
                }
            };
        let cached: astra_pipeline::step_protocol::CachedToolResult =
            match serde_json::from_str(&result_json) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        dedup_key = %_dedup_key,
                        "failed to deserialize exactly-once result from DB; skipping entry"
                    );
                    continue;
                }
            };
        executor.cache_mut().record(&key, cached);
    }

    if count > 0 {
        tracing::info!(
            user_id = %user_id,
            session_id = %session_id,
            count,
            "warmed exactly-once cache from DB"
        );
    }
    Ok(())
}

// ── Cache lookup ───────────────────────────────────────────────────────────

pub(crate) fn public_tool_arguments(args: &Value) -> Value {
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    Value::Object(
        map.iter()
            .filter(|(key, _)| !key.starts_with('_'))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

fn exactly_once_key(name: &str, args: &Value) -> astra_pipeline::step_protocol::IdempotencyKey {
    let public_args = public_tool_arguments(args);
    astra_pipeline::step_protocol::IdempotencyKey::semantic(name, &public_args)
}

pub(crate) fn check_cache(
    state: Option<&ExactlyOnceState>,
    name: &str,
    args: &Value,
) -> Option<astra_tools::ToolResult> {
    use astra_turn_types::classify_tool_idempotency;

    let state = state?;
    let public_args = public_tool_arguments(args);
    let key = exactly_once_key(name, args);
    let cache_key = key.cache_key();

    let executor = match state.in_memory.lock() {
        Ok(executor) => executor,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "exactly-once cache lock poisoned; skipping cache lookup"
            );
            return None;
        }
    };
    let cached = executor.cache().check(&key)?;
    let idempotency = classify_tool_idempotency(name, Some(&public_args));

    match idempotency {
        astra_turn_types::ToolIdempotency::NonIdempotent
        | astra_turn_types::ToolIdempotency::IdempotentWrite => {
            tracing::debug!(
                tool_name = %name,
                cache_key = %cache_key,
                "Exactly-once: cache hit, returning cached result"
            );
            Some(cached_tool_result(cached))
        }
        astra_turn_types::ToolIdempotency::PureRead => {
            tracing::debug!(
                tool_name = %name,
                cache_key = %cache_key,
                "Exactly-once: cache hit for PureRead (AlwaysCache policy)"
            );
            Some(cached_tool_result(cached))
        }
    }
}

fn cached_tool_result(
    cached: &astra_pipeline::step_protocol::CachedToolResult,
) -> astra_tools::ToolResult {
    astra_tools::ToolResult {
        output: cached.output.clone(),
        is_error: cached.is_error,
        metadata: None,
        exit_semantics: None,
    }
}

// ── Result recording ───────────────────────────────────────────────────────

pub(crate) async fn record_result(
    state: Option<&ExactlyOnceState>,
    name: &str,
    args: &Value,
    result: &astra_tools::ToolResult,
) {
    use astra_pipeline::step_protocol::CachedToolResult;

    if result.is_error {
        tracing::debug!(
            tool_name = %name,
            "Exactly-once: skipping failed tool result because failures are retryable"
        );
        return;
    }

    let Some(state) = state else {
        return;
    };

    let key = exactly_once_key(name, args);
    let cache_key = key.cache_key();

    let cached_at = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "system clock predates UNIX_EPOCH; using zero exactly-once cache timestamp"
            );
            0
        }
    };

    // ── In-memory (always) ─────────────────────────────────────────────
    {
        let mut executor = match state.in_memory.lock() {
            Ok(executor) => executor,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "exactly-once cache lock poisoned; skipping result record"
                );
                return;
            }
        };
        let cached_result = CachedToolResult {
            tool_name: name.to_string(),
            output: result.output.clone(),
            is_error: false,
            cached_at,
            context_signature: None,
        };
        executor.cache_mut().record(&key, cached_result);
    }

    tracing::debug!(
        tool_name = %name,
        cache_key = %cache_key,
        "Exactly-once: recorded successful tool result in cache"
    );

    // ── DB persistence (best-effort, short timeout) ────────────────────
    // Previously fire-and-forget via tokio::spawn — a crash between
    // returning the result and the spawned task completing would lose
    // the idempotency record, causing re-execution on restart.
    // Now we await the DB write directly with a short timeout; if the
    // DB is slow or unavailable the in-memory cache still serves the
    // current session.
    if let Some(ref pool) = state.pool {
        let pool = pool.clone();
        let user_id = state.user_id.clone();
        let session_id = state.session_id.clone();
        let dedup_key = dedup_key_hash(&cache_key);
        let cached_for_persist = CachedToolResult {
            tool_name: name.to_string(),
            output: result.output.clone(),
            is_error: false,
            cached_at,
            context_signature: None,
        };
        tracing::debug!(
            tool_name = %name,
            cache_key = %cache_key,
            dedup_key = %dedup_key,
            "Exactly-once: persisting result for crash recovery"
        );
        let persist_fut = persist_result(
            &pool,
            &user_id,
            &session_id,
            &dedup_key,
            &key,
            &cached_for_persist,
        );
        match tokio::time::timeout(Duration::from_millis(500), persist_fut).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    user_id = %user_id,
                    session_id = %session_id,
                    dedup_key = %dedup_key,
                    error = %e,
                    "failed to persist exactly-once result to DB"
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    user_id = %user_id,
                    session_id = %session_id,
                    dedup_key = %dedup_key,
                    "timed out persisting exactly-once result to DB"
                );
            }
        }
    }
}

async fn persist_result(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    dedup_key: &str,
    key: &astra_pipeline::step_protocol::IdempotencyKey,
    result: &astra_pipeline::step_protocol::CachedToolResult,
) -> Result<(), String> {
    let key_json = serialize_exactly_once_json("key_json", key)?;
    let result_json = serialize_exactly_once_json("result_json", result)?;

    // Use INSERT IGNORE to handle race: if the key already exists (e.g. from
    // a replay or concurrent execution), silently skip the insert rather than
    // failing with a duplicate-key error.
    sqlx::query(
        "INSERT IGNORE INTO tool_exactly_once_results \
         (user_id, session_id, dedup_key, key_json, result_json, recorded_at)
         VALUES (?, ?, ?, ?, ?, UNIX_TIMESTAMP() * 1000)",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(dedup_key)
    .bind(&key_json)
    .bind(&result_json)
    .execute(pool.get())
    .await
    .map_err(|source| format!("persist exactly-once result: {source}"))?;

    Ok(())
}

fn serialize_exactly_once_json<T: Serialize>(
    label: &'static str,
    value: &T,
) -> Result<String, String> {
    serde_json::to_string(value)
        .map_err(|source| format!("serialize exactly-once {label}: {source}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serializer;
    use serde_json::json;

    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(serde::ser::Error::custom("forced serializer failure"))
        }
    }

    #[test]
    fn exactly_once_json_serialization_fails_loudly() {
        let error = serialize_exactly_once_json("key_json", &FailingSerialize)
            .expect_err("serializer errors must be surfaced");

        assert!(
            error.contains("serialize exactly-once key_json")
                && error.contains("forced serializer failure"),
            "serialization error should identify the affected exactly-once field: {error}"
        );
    }

    #[test]
    fn exactly_once_json_serialization_round_trips_cache_payloads() {
        let args = json!({"command": "date"});
        let key = astra_pipeline::step_protocol::IdempotencyKey::semantic("shell", &args);
        let cached = astra_pipeline::step_protocol::CachedToolResult {
            tool_name: "shell".to_string(),
            output: "Fri Jun 26".to_string(),
            is_error: false,
            cached_at: 42,
            context_signature: None,
        };

        let key_json = serialize_exactly_once_json("key_json", &key).expect("serialize key");
        let result_json =
            serialize_exactly_once_json("result_json", &cached).expect("serialize cached result");

        assert!(
            !key_json.is_empty(),
            "key_json must not silently become empty"
        );
        assert!(
            !result_json.is_empty(),
            "result_json must not silently become empty"
        );
        serde_json::from_str::<astra_pipeline::step_protocol::IdempotencyKey>(&key_json)
            .expect("key JSON should deserialize");
        let restored =
            serde_json::from_str::<astra_pipeline::step_protocol::CachedToolResult>(&result_json)
                .expect("cached result JSON should deserialize");
        assert_eq!(restored.output, "Fri Jun 26");
        assert!(!restored.is_error);
    }
}
