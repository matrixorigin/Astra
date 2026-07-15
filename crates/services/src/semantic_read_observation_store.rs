//! MatrixOne-backed semantic read-observation cache.
//!
//! Per-session operations serialize on the authoritative session row. This
//! makes entry, byte, and fill limits hard across runtime processes without a
//! second eventually-consistent budget counter.

use astra_core::SharedPool;
use astra_turn_types::{
    SemanticReadCacheKey, SemanticReadCacheLimits, SemanticReadCacheLookup, SemanticReadObservation,
};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;

const MAX_FILL_OWNER_BYTES: usize = 64;

#[derive(Clone)]
pub struct DatabaseSemanticReadObservationStore {
    pool: SharedPool,
    limits: SemanticReadCacheLimits,
}

impl DatabaseSemanticReadObservationStore {
    pub fn new(
        pool: SharedPool,
        limits: SemanticReadCacheLimits,
    ) -> Result<Self, SemanticReadObservationStoreError> {
        Ok(Self {
            pool,
            limits: limits.validate()?,
        })
    }

    pub async fn lookup_or_claim(
        &self,
        user_id: &str,
        session_id: &str,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
        fill_lease_duration_ms: u64,
    ) -> Result<SemanticReadCacheLookup, SemanticReadObservationStoreError> {
        validate_owner(fill_owner)?;
        key.validate()?;
        let lease_duration_us = lease_duration_us(fill_lease_duration_ms)?;
        let key_json = serde_json::to_string(key).map_err(|source| {
            SemanticReadObservationStoreError::Serialization {
                field: "key_json",
                source,
            }
        })?;
        let mut tx = self.pool.get().begin().await?;
        lock_session(&mut tx, user_id, session_id).await?;
        sqlx::query(
            "DELETE FROM semantic_read_observations
             WHERE user_id = ? AND session_id = ? AND state = 'filling'
               AND fill_lease_expires_at <= CURRENT_TIMESTAMP(6)",
        )
        .bind(user_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await?;

        if let Some(row) = load_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await? {
            let stored_key = match decode_key(&row.key_json) {
                Ok(stored_key) => stored_key,
                Err(error) => {
                    remove_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await?;
                    tx.commit().await?;
                    return Err(error);
                }
            };
            if stored_key != *key {
                remove_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await?;
                tx.commit().await?;
                return Err(SemanticReadObservationStoreError::CacheKeyCollisionRemoved);
            }
            match row.state.as_str() {
                "ready" => {
                    let observation = match decode_ready_observation(&row, key) {
                        Ok(observation) => observation,
                        Err(error) => {
                            remove_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await?;
                            tx.commit().await?;
                            return Err(error);
                        }
                    };
                    sqlx::query(
                        "UPDATE semantic_read_observations
                         SET last_accessed_at = CURRENT_TIMESTAMP(6),
                             updated_at = CURRENT_TIMESTAMP(6)
                         WHERE user_id = ? AND session_id = ? AND key_id = ?
                           AND state = 'ready'",
                    )
                    .bind(user_id)
                    .bind(session_id)
                    .bind(&key.key_id)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    return Ok(SemanticReadCacheLookup::Hit(Box::new(observation)));
                }
                "filling" => {
                    let expires_at = match filling_entry_expiry(&row) {
                        Ok(expires_at) => expires_at,
                        Err(error) => {
                            remove_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await?;
                            tx.commit().await?;
                            return Err(error);
                        }
                    };
                    tx.commit().await?;
                    return Ok(SemanticReadCacheLookup::FillInProgress {
                        lease_expires_at_epoch_ms: expires_at,
                    });
                }
                other => {
                    remove_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await?;
                    tx.commit().await?;
                    return Err(SemanticReadObservationStoreError::InvalidStateRemoved(
                        other.to_string(),
                    ));
                }
            }
        }

        let in_flight: u64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM semantic_read_observations
             WHERE user_id = ? AND session_id = ? AND state = 'filling'",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        if in_flight >= self.limits.max_in_flight_fills as u64 {
            tx.commit().await?;
            return Ok(SemanticReadCacheLookup::FillCapacityExceeded);
        }

        sqlx::query(
            "INSERT INTO semantic_read_observations (
                user_id, session_id, key_id, key_json, state,
                fill_owner, fill_lease_expires_at,
                created_at, updated_at, last_accessed_at
             ) VALUES (?, ?, ?, ?, 'filling', ?,
                       TIMESTAMPADD(MICROSECOND, ?, CURRENT_TIMESTAMP(6)),
                       CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6))",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(&key.key_id)
        .bind(key_json)
        .bind(fill_owner)
        .bind(lease_duration_us)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SemanticReadCacheLookup::FillClaimed)
    }

    pub async fn complete_fill(
        &self,
        user_id: &str,
        session_id: &str,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
        observation: &SemanticReadObservation,
    ) -> Result<(), SemanticReadObservationStoreError> {
        validate_owner(fill_owner)?;
        key.validate()?;
        observation.validate()?;
        if observation.key != *key {
            return Err(SemanticReadObservationStoreError::ObservationKeyMismatch);
        }
        let observation_bytes = observation.encoded_len()?;
        let observation_bytes_u64 = u64::try_from(observation_bytes)
            .map_err(|_| SemanticReadObservationStoreError::ObservationSizeOverflow)?;
        let observation_json = serde_json::to_string(observation).map_err(|source| {
            SemanticReadObservationStoreError::Serialization {
                field: "observation_json",
                source,
            }
        })?;
        let mut tx = self.pool.get().begin().await?;
        lock_session(&mut tx, user_id, session_id).await?;
        let row = load_entry_in_tx(&mut tx, user_id, session_id, &key.key_id)
            .await?
            .ok_or(SemanticReadObservationStoreError::MissingFill)?;
        let stored_key = match decode_key(&row.key_json) {
            Ok(stored_key) => stored_key,
            Err(error) => {
                remove_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await?;
                tx.commit().await?;
                return Err(error);
            }
        };
        if stored_key != *key {
            remove_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await?;
            tx.commit().await?;
            return Err(SemanticReadObservationStoreError::CacheKeyCollisionRemoved);
        }
        ensure_active_fill(&row, fill_owner)?;

        if observation_bytes > self.limits.max_ready_bytes {
            remove_entry_in_tx(&mut tx, user_id, session_id, &key.key_id).await?;
            tx.commit().await?;
            return Err(
                SemanticReadObservationStoreError::ObservationExceedsStoreCapacity {
                    observation_bytes,
                    max_ready_bytes: self.limits.max_ready_bytes,
                },
            );
        }

        let (mut ready_entries, mut ready_bytes) =
            ready_usage(&mut tx, user_id, session_id).await?;
        while ready_entries >= self.limits.max_ready_entries as u64
            || ready_bytes.saturating_add(observation_bytes_u64)
                > self.limits.max_ready_bytes as u64
        {
            let evicted = evict_oldest_ready(&mut tx, user_id, session_id).await?;
            let Some(evicted_bytes) = evicted else {
                rollback(tx, "semantic cache ready-capacity invariant").await;
                return Err(SemanticReadObservationStoreError::ReadyCapacityInvariant);
            };
            ready_entries = ready_entries.saturating_sub(1);
            ready_bytes = ready_bytes.saturating_sub(evicted_bytes);
        }

        let updated = sqlx::query(
            "UPDATE semantic_read_observations
             SET state = 'ready', fill_owner = NULL, fill_lease_expires_at = NULL,
                 observation_json = ?, observation_bytes = ?,
                 updated_at = CURRENT_TIMESTAMP(6), last_accessed_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND key_id = ?
               AND state = 'filling' AND fill_owner = ?
               AND fill_lease_expires_at > CURRENT_TIMESTAMP(6)",
        )
        .bind(observation_json)
        .bind(observation_bytes_u64)
        .bind(user_id)
        .bind(session_id)
        .bind(&key.key_id)
        .bind(fill_owner)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            rollback(tx, "semantic cache complete-fill owner/lease mismatch").await;
            return Err(SemanticReadObservationStoreError::FillOwnerOrLeaseMismatch);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn abandon_fill(
        &self,
        user_id: &str,
        session_id: &str,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
    ) -> Result<(), SemanticReadObservationStoreError> {
        validate_owner(fill_owner)?;
        key.validate()?;
        let mut tx = self.pool.get().begin().await?;
        lock_session(&mut tx, user_id, session_id).await?;
        let deleted = sqlx::query(
            "DELETE FROM semantic_read_observations
             WHERE user_id = ? AND session_id = ? AND key_id = ?
               AND state = 'filling' AND fill_owner = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(&key.key_id)
        .bind(fill_owner)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if deleted != 1 {
            rollback(tx, "semantic cache abandon-fill mismatch").await;
            return Err(SemanticReadObservationStoreError::FillOwnerOrLeaseMismatch);
        }
        tx.commit().await?;
        Ok(())
    }
}

struct StoredEntry {
    state: String,
    key_json: String,
    fill_owner: Option<String>,
    fill_lease_expires_at_epoch_ms: Option<u64>,
    observation_json: Option<String>,
    observation_bytes: Option<u64>,
}

async fn lock_session(
    tx: &mut Transaction<'_, MySql>,
    user_id: &str,
    session_id: &str,
) -> Result<(), SemanticReadObservationStoreError> {
    let exists: Option<String> = sqlx::query_scalar(
        "SELECT session_id FROM agent_sessions
         WHERE session_id = ? AND user_id = ? FOR UPDATE",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    if exists.is_none() {
        return Err(SemanticReadObservationStoreError::SessionNotFound);
    }
    Ok(())
}

async fn load_entry_in_tx(
    tx: &mut Transaction<'_, MySql>,
    user_id: &str,
    session_id: &str,
    key_id: &str,
) -> Result<Option<StoredEntry>, SemanticReadObservationStoreError> {
    let row = sqlx::query(
        "SELECT state, CAST(key_json AS CHAR) AS key_json, fill_owner,
                CAST(UNIX_TIMESTAMP(fill_lease_expires_at) * 1000 AS UNSIGNED)
                    AS fill_lease_expires_at_epoch_ms,
                CAST(observation_json AS CHAR) AS observation_json,
                observation_bytes
         FROM semantic_read_observations
         WHERE user_id = ? AND session_id = ? AND key_id = ? FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(key_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        Ok(StoredEntry {
            state: row.try_get("state")?,
            key_json: row.try_get("key_json")?,
            fill_owner: row.try_get("fill_owner")?,
            fill_lease_expires_at_epoch_ms: row.try_get("fill_lease_expires_at_epoch_ms")?,
            observation_json: row.try_get("observation_json")?,
            observation_bytes: row.try_get("observation_bytes")?,
        })
    })
    .transpose()
}

fn decode_key(value: &str) -> Result<SemanticReadCacheKey, SemanticReadObservationStoreError> {
    serde_json::from_str(value).map_err(|source| {
        SemanticReadObservationStoreError::InvalidStoredJsonRemoved {
            field: "key_json",
            source,
        }
    })
}

fn decode_ready_observation(
    row: &StoredEntry,
    key: &SemanticReadCacheKey,
) -> Result<SemanticReadObservation, SemanticReadObservationStoreError> {
    if row.fill_owner.is_some() || row.fill_lease_expires_at_epoch_ms.is_some() {
        return Err(SemanticReadObservationStoreError::InvalidReadyEntryRemoved);
    }
    let encoded = row
        .observation_json
        .as_deref()
        .ok_or(SemanticReadObservationStoreError::InvalidReadyEntryRemoved)?;
    let observation: SemanticReadObservation = serde_json::from_str(encoded).map_err(|source| {
        SemanticReadObservationStoreError::InvalidStoredJsonRemoved {
            field: "observation_json",
            source,
        }
    })?;
    if observation.key != *key {
        return Err(SemanticReadObservationStoreError::ObservationKeyMismatch);
    }
    let encoded_bytes = observation.encoded_len()?;
    if row.observation_bytes != Some(encoded_bytes as u64) {
        return Err(SemanticReadObservationStoreError::ObservationSizeMismatchRemoved);
    }
    Ok(observation)
}

fn ensure_active_fill(
    row: &StoredEntry,
    fill_owner: &str,
) -> Result<(), SemanticReadObservationStoreError> {
    if row.state != "filling"
        || row.fill_owner.as_deref() != Some(fill_owner)
        || row.fill_lease_expires_at_epoch_ms.is_none()
        || row.observation_json.is_some()
        || row.observation_bytes.is_some()
    {
        return Err(SemanticReadObservationStoreError::FillOwnerOrLeaseMismatch);
    }
    Ok(())
}

fn filling_entry_expiry(row: &StoredEntry) -> Result<u64, SemanticReadObservationStoreError> {
    if row.fill_owner.as_deref().is_none_or(str::is_empty)
        || row.observation_json.is_some()
        || row.observation_bytes.is_some()
    {
        return Err(SemanticReadObservationStoreError::InvalidFillingEntryRemoved);
    }
    row.fill_lease_expires_at_epoch_ms
        .ok_or(SemanticReadObservationStoreError::InvalidFillingEntryRemoved)
}

async fn ready_usage(
    tx: &mut Transaction<'_, MySql>,
    user_id: &str,
    session_id: &str,
) -> Result<(u64, u64), SemanticReadObservationStoreError> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS ready_entries,
                CAST(COALESCE(SUM(observation_bytes), 0) AS UNSIGNED) AS ready_bytes
         FROM semantic_read_observations
         WHERE user_id = ? AND session_id = ? AND state = 'ready'",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok((row.try_get("ready_entries")?, row.try_get("ready_bytes")?))
}

async fn evict_oldest_ready(
    tx: &mut Transaction<'_, MySql>,
    user_id: &str,
    session_id: &str,
) -> Result<Option<u64>, SemanticReadObservationStoreError> {
    let row = sqlx::query(
        "SELECT key_id, observation_bytes
         FROM semantic_read_observations
         WHERE user_id = ? AND session_id = ? AND state = 'ready'
         ORDER BY last_accessed_at ASC, key_id ASC LIMIT 1 FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let key_id: String = row.try_get("key_id")?;
    let bytes: u64 = row.try_get("observation_bytes")?;
    sqlx::query(
        "DELETE FROM semantic_read_observations
         WHERE user_id = ? AND session_id = ? AND key_id = ? AND state = 'ready'",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(key_id)
    .execute(&mut **tx)
    .await?;
    Ok(Some(bytes))
}

async fn remove_entry_in_tx(
    tx: &mut Transaction<'_, MySql>,
    user_id: &str,
    session_id: &str,
    key_id: &str,
) -> Result<(), SemanticReadObservationStoreError> {
    sqlx::query(
        "DELETE FROM semantic_read_observations
         WHERE user_id = ? AND session_id = ? AND key_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(key_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn validate_owner(fill_owner: &str) -> Result<(), SemanticReadObservationStoreError> {
    if fill_owner.trim().is_empty() {
        return Err(SemanticReadObservationStoreError::EmptyFillOwner);
    }
    if fill_owner.len() > MAX_FILL_OWNER_BYTES {
        return Err(SemanticReadObservationStoreError::FillOwnerTooLong {
            actual_bytes: fill_owner.len(),
            max_bytes: MAX_FILL_OWNER_BYTES,
        });
    }
    Ok(())
}

fn lease_duration_us(value: u64) -> Result<i64, SemanticReadObservationStoreError> {
    if value == 0 {
        return Err(SemanticReadObservationStoreError::InvalidFillLeaseDuration);
    }
    value
        .checked_mul(1_000)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(SemanticReadObservationStoreError::FillLeaseDurationOverflow)
}

async fn rollback(tx: Transaction<'_, MySql>, context: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(context, %error, "semantic read observation transaction rollback failed");
    }
}

#[derive(Debug, Error)]
pub enum SemanticReadObservationStoreError {
    #[error("semantic read observation database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("semantic read observation session does not exist or is not owned by the caller")]
    SessionNotFound,
    #[error("semantic read cache fill owner must not be empty")]
    EmptyFillOwner,
    #[error(
        "semantic read cache fill owner is {actual_bytes} bytes but the limit is {max_bytes} bytes"
    )]
    FillOwnerTooLong {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("semantic read cache fill lease duration must be positive")]
    InvalidFillLeaseDuration,
    #[error("semantic read cache fill lease duration exceeds database range")]
    FillLeaseDurationOverflow,
    #[error("serialize semantic read observation {field}: {source}")]
    Serialization {
        field: &'static str,
        source: serde_json::Error,
    },
    #[error("invalid stored semantic read observation {field} was removed: {source}")]
    InvalidStoredJsonRemoved {
        field: &'static str,
        source: serde_json::Error,
    },
    #[error("semantic read cache key collision was removed")]
    CacheKeyCollisionRemoved,
    #[error("semantic read observation does not match its cache key")]
    ObservationKeyMismatch,
    #[error("semantic read observation stored byte count did not match and was removed")]
    ObservationSizeMismatchRemoved,
    #[error("semantic read cache ready entry was structurally invalid and was removed")]
    InvalidReadyEntryRemoved,
    #[error("semantic read cache entry had invalid state '{0}' and was removed")]
    InvalidStateRemoved(String),
    #[error("semantic read cache fill entry was structurally invalid and was removed")]
    InvalidFillingEntryRemoved,
    #[error("semantic read cache fill does not exist")]
    MissingFill,
    #[error("semantic read cache fill owner or lease no longer authorizes completion")]
    FillOwnerOrLeaseMismatch,
    #[error("semantic read observation size exceeds the database integer range")]
    ObservationSizeOverflow,
    #[error(
        "semantic read observation is {observation_bytes} bytes but this session permits {max_ready_bytes} ready bytes"
    )]
    ObservationExceedsStoreCapacity {
        observation_bytes: usize,
        max_ready_bytes: usize,
    },
    #[error("semantic read cache ready-capacity accounting has no evictable entry")]
    ReadyCapacityInvariant,
    #[error(transparent)]
    Contract(#[from] astra_turn_types::SemanticReadCacheContractError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_fill_owner_fail_before_database_access() {
        assert!(
            SemanticReadCacheLimits {
                max_ready_entries: 0,
                ..SemanticReadCacheLimits::default()
            }
            .validate()
            .is_err()
        );
        assert!(matches!(
            validate_owner(" "),
            Err(SemanticReadObservationStoreError::EmptyFillOwner)
        ));
        assert!(matches!(
            lease_duration_us(0),
            Err(SemanticReadObservationStoreError::InvalidFillLeaseDuration)
        ));
    }
}
