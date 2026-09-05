//! Personal Model Access authority for Runner-local inference bindings.
//!
//! Presence is owned by `edge_agent_registry`. Publications are immutable
//! operation receipts, not another connection inventory. A live authenticated
//! registration serializes journal enrollment and publication. Once admitted,
//! an inference attempt retains its own immutable authority across reconnects.

use astra_core::SharedPool;
use astra_turn_types::runner_inference::{
    RUNNER_INFERENCE_PROTOCOL_VERSION, RunnerInferenceBindingChange,
    RunnerInferenceBindingDefinition, RunnerInferenceBindingIdentity,
    RunnerInferenceBindingPublication, RunnerInferenceBindingReceipt, RunnerInferenceId,
};
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};

use crate::service_error::{ServiceError, ServiceErrorKind, ServiceResult};

/// Stable personal Offering target. Publication/profile revisions refresh the
/// execution material without changing a user's exact model selection. A lost
/// journal is a new target and cannot inherit old selection authority.
pub fn runner_offering_id(user_id: &str, identity: &RunnerInferenceBindingIdentity) -> String {
    let mut hash = Sha256::new();
    for field in [
        user_id,
        identity.runner_id.as_str(),
        identity.journal_id.as_str(),
        identity.binding_id.as_str(),
    ] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    format!("runner-{}", &format!("{:x}", hash.finalize())[..48])
}

impl ResolvedRunnerModelBinding {
    pub fn catalog_item(&self) -> crate::models::ModelListItem {
        crate::models::ModelListItem {
            offering_id: runner_offering_id(&self.user_id, &self.definition.identity),
            access_id: format!("runner-{}", self.definition.identity.runner_id.as_str()),
            access_kind: crate::models::ModelAccessKind::ThisDevice,
            access_label: "This device".into(),
            execution_placement: crate::models::ModelExecutionPlacement::Edge,
            name: self.definition.display_name.as_str().into(),
            provider: "openai".into(),
            description: None,
            is_active: true,
            context_window: self.definition.context_window.get() as i32,
            max_completion_tokens: Some(self.definition.max_output_tokens.get() as i32),
            architecture: None,
            thinking_capability: None,
        }
    }
}

/// Stable catalog projection. Availability is deliberately separate from the
/// immutable published definition: heartbeat expiry may make a binding
/// unselectable, but it must not make the user's configured model disappear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerModelCatalogBinding {
    pub user_id: String,
    pub definition: RunnerInferenceBindingDefinition,
    pub online: bool,
}

impl RunnerModelCatalogBinding {
    pub fn catalog_item(&self) -> crate::models::ModelListItem {
        crate::models::ModelListItem {
            offering_id: runner_offering_id(&self.user_id, &self.definition.identity),
            access_id: format!("runner-{}", self.definition.identity.runner_id.as_str()),
            access_kind: crate::models::ModelAccessKind::ThisDevice,
            // Server cannot prove that an arbitrary Web/CLI consumer is
            // colocated with this Runner. Local surfaces may render “This
            // device” only after their private host handshake.
            access_label: "Personal Runner".into(),
            execution_placement: crate::models::ModelExecutionPlacement::Edge,
            name: self.definition.display_name.as_str().into(),
            provider: "openai".into(),
            description: (!self.online)
                .then(|| "Runner offline — reconnect this device to use the model".into()),
            is_active: self.online,
            context_window: self.definition.context_window.get() as i32,
            max_completion_tokens: Some(self.definition.max_output_tokens.get() as i32),
            architecture: None,
            thinking_capability: None,
        }
    }
}

/// Derived from authenticated registry ownership, never deserialized from a
/// publication. The connection identity fences new control operations only.
#[derive(Clone, Debug)]
pub struct AuthenticatedRunnerConnection {
    pub user_id: String,
    pub runner_id: RunnerInferenceId,
    pub edge_id: String,
}

/// Resolution is personal and revision-pinned. Only this owner's currently
/// enrolled executor may receive a new grant. The boot nonce is not a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRunnerModelBinding {
    pub user_id: String,
    pub definition: RunnerInferenceBindingDefinition,
    pub process_boot_nonce: RunnerInferenceId,
    pub publication_revision: u64,
}

fn storage(error: sqlx::Error) -> ServiceError {
    ServiceError::with_source(
        ServiceErrorKind::Persistence,
        "Runner binding storage",
        error,
    )
}

pub(crate) async fn lock_connection(
    tx: &mut Transaction<'_, MySql>,
    connection: &AuthenticatedRunnerConnection,
) -> ServiceResult<sqlx::mysql::MySqlRow> {
    sqlx::query(
        "SELECT inference_journal_id, inference_boot_nonce, inference_edge_id,
                inference_publication_revision
         FROM edge_agent_registry
         WHERE user_id = ? AND edge_agent_id = ? AND edge_id = ?
           AND registration_state = 1
           AND last_heartbeat_at > DATE_SUB(NOW(6), INTERVAL 60 SECOND)
         FOR UPDATE",
    )
    .bind(&connection.user_id)
    .bind(connection.runner_id.as_str())
    .bind(&connection.edge_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(storage)?
    .ok_or_else(|| ServiceError::conflict("Runner registration is absent, stale or superseded"))
}

/// Called only after the concrete inference executor and local journal are
/// ready. An ordinary tool-capability publication never calls this method.
pub async fn enroll_runner_inference(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    protocol_version: u16,
    journal_id: &RunnerInferenceId,
    process_boot_nonce: &RunnerInferenceId,
) -> ServiceResult<()> {
    if protocol_version != RUNNER_INFERENCE_PROTOCOL_VERSION {
        return Err(ServiceError::invalid(
            "Runner inference protocol unsupported",
        ));
    }
    let mut tx = pool.get().begin().await.map_err(storage)?;
    let row = lock_connection(&mut tx, connection).await?;
    let previous: Option<String> = row.try_get("inference_journal_id").map_err(storage)?;
    let revision: i64 = if previous.as_deref() == Some(journal_id.as_str()) {
        row.try_get("inference_publication_revision")
            .map_err(storage)?
    } else {
        if sqlx::query(
            "SELECT 1 FROM runner_model_binding_publications
             WHERE user_id = ? AND runner_id = ? AND journal_id = ? LIMIT 1",
        )
        .bind(&connection.user_id)
        .bind(connection.runner_id.as_str())
        .bind(journal_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .is_some()
        {
            return Err(ServiceError::conflict(
                "retired Runner journal cannot be re-enrolled",
            ));
        }
        // A journal reset retires its inventory; it cannot reconstruct old
        // attempts or accept old publication operations into the new journal.
        0
    };
    sqlx::query(
        "UPDATE edge_agent_registry SET inference_journal_id = ?, inference_boot_nonce = ?,
             inference_edge_id = ?, inference_publication_revision = ?
         WHERE user_id = ? AND edge_agent_id = ? AND edge_id = ?",
    )
    .bind(journal_id.as_str())
    .bind(process_boot_nonce.as_str())
    .bind(&connection.edge_id)
    .bind(revision)
    .bind(&connection.user_id)
    .bind(connection.runner_id.as_str())
    .bind(&connection.edge_id)
    .execute(&mut *tx)
    .await
    .map_err(storage)?;
    tx.commit().await.map_err(storage)
}

fn receipt(
    publication: &RunnerInferenceBindingPublication,
    revision: i64,
) -> ServiceResult<RunnerInferenceBindingReceipt> {
    let revision = u64::try_from(revision)
        .ok()
        .and_then(std::num::NonZeroU64::new)
        .ok_or_else(|| ServiceError::invalid("invalid durable publication revision"))?;
    Ok(RunnerInferenceBindingReceipt {
        operation_id: publication.operation_id.clone(),
        publication_revision: revision,
        identity: publication.change.identity().clone(),
    })
}

fn revision_i64(value: u64) -> ServiceResult<i64> {
    i64::try_from(value).map_err(|_| ServiceError::invalid("Runner revision exceeds durable range"))
}

/// CAS one binding, retaining every operation receipt for journal-lifetime
/// replay. An old operation can return its receipt but cannot roll back current
/// inventory. Reusing an operation ID for different contents fails closed.
pub async fn publish_runner_binding(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    publication: &RunnerInferenceBindingPublication,
) -> ServiceResult<RunnerInferenceBindingReceipt> {
    let identity = publication.change.identity();
    if publication.protocol_version != RUNNER_INFERENCE_PROTOCOL_VERSION
        || identity.runner_id != connection.runner_id
    {
        return Err(ServiceError::invalid(
            "Runner publication identity or protocol mismatch",
        ));
    }
    let expected_revision = revision_i64(publication.expected_publication_revision)?;
    let binding_revision = revision_i64(identity.binding_revision.get())?;
    let profile_revision = revision_i64(identity.profile_revision.get())?;
    let encoded = serde_json::to_vec(publication)
        .map_err(|_| ServiceError::invalid("Runner publication encoding failed"))?;
    let operation_hash = format!("{:x}", Sha256::digest(&encoded));
    let mut tx = pool.get().begin().await.map_err(storage)?;
    let registry = lock_connection(&mut tx, connection).await?;
    let journal: Option<String> = registry.try_get("inference_journal_id").map_err(storage)?;
    let enrolled_edge: Option<String> = registry.try_get("inference_edge_id").map_err(storage)?;
    if journal.as_deref() != Some(identity.journal_id.as_str())
        || enrolled_edge.as_deref() != Some(connection.edge_id.as_str())
    {
        return Err(ServiceError::conflict(
            "Runner inference executor is not enrolled",
        ));
    }
    if let Some(previous) = sqlx::query(
        "SELECT operation_hash, publication_revision FROM runner_model_binding_publications
         WHERE user_id = ? AND runner_id = ? AND journal_id = ? AND operation_id = ?",
    )
    .bind(&connection.user_id)
    .bind(identity.runner_id.as_str())
    .bind(identity.journal_id.as_str())
    .bind(publication.operation_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(storage)?
    {
        if previous
            .try_get::<String, _>("operation_hash")
            .map_err(storage)?
            != operation_hash
        {
            return Err(ServiceError::conflict(
                "Runner publication operation content conflicts",
            ));
        }
        return receipt(
            publication,
            previous.try_get("publication_revision").map_err(storage)?,
        );
    }
    let current: i64 = registry
        .try_get("inference_publication_revision")
        .map_err(storage)?;
    if current != expected_revision {
        return Err(ServiceError::conflict(
            "Runner publication revision changed",
        ));
    }
    let previous = sqlx::query(
        "SELECT binding_revision, profile_revision, enabled FROM runner_model_binding_publications
         WHERE user_id = ? AND runner_id = ? AND journal_id = ? AND binding_id = ?
         ORDER BY publication_revision DESC LIMIT 1",
    )
    .bind(&connection.user_id)
    .bind(identity.runner_id.as_str())
    .bind(identity.journal_id.as_str())
    .bind(identity.binding_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(storage)?;
    let previous_revision = previous
        .as_ref()
        .map(|row| row.try_get::<i64, _>("binding_revision"))
        .transpose()
        .map_err(storage)?;
    let (enabled, definition) = match &publication.change {
        RunnerInferenceBindingChange::Publish { definition } => {
            if definition.max_output_tokens > definition.context_window
                || definition.context_window.get() > i32::MAX as u32
            {
                return Err(ServiceError::invalid(
                    "Runner output bound exceeds context window",
                ));
            }
            if binding_revision
                != previous_revision
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or_else(|| ServiceError::conflict("Runner binding revision exhausted"))?
            {
                return Err(ServiceError::conflict(
                    "Runner binding revision must advance exactly once",
                ));
            }
            (
                true,
                Some(
                    serde_json::to_string(definition)
                        .map_err(|_| ServiceError::invalid("Runner definition encoding failed"))?,
                ),
            )
        }
        RunnerInferenceBindingChange::Disable { .. } => {
            let previous =
                previous.ok_or_else(|| ServiceError::not_found("Runner binding absent"))?;
            if previous_revision != Some(binding_revision)
                || previous
                    .try_get::<i64, _>("profile_revision")
                    .map_err(storage)?
                    != profile_revision
                || !previous.try_get::<bool, _>("enabled").map_err(storage)?
            {
                return Err(ServiceError::conflict(
                    "Runner disable must match the enabled binding",
                ));
            }
            (false, None)
        }
    };
    let next = current
        .checked_add(1)
        .ok_or_else(|| ServiceError::conflict("Runner publication revision exhausted"))?;
    sqlx::query(
        "INSERT INTO runner_model_binding_publications
         (user_id, runner_id, journal_id, publication_revision, operation_id, operation_hash,
          binding_id, binding_revision, profile_revision, enabled, definition_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&connection.user_id)
    .bind(identity.runner_id.as_str())
    .bind(identity.journal_id.as_str())
    .bind(next)
    .bind(publication.operation_id.as_str())
    .bind(operation_hash)
    .bind(identity.binding_id.as_str())
    .bind(binding_revision)
    .bind(profile_revision)
    .bind(enabled)
    .bind(definition)
    .execute(&mut *tx)
    .await
    .map_err(storage)?;
    sqlx::query(
        "UPDATE edge_agent_registry SET inference_publication_revision = ?
         WHERE user_id = ? AND edge_agent_id = ? AND edge_id = ?",
    )
    .bind(next)
    .bind(&connection.user_id)
    .bind(identity.runner_id.as_str())
    .bind(&connection.edge_id)
    .execute(&mut *tx)
    .await
    .map_err(storage)?;
    tx.commit().await.map_err(storage)?;
    receipt(publication, next)
}

/// Used again under the admission transaction. A UI resolution is not authority
/// to issue a later grant after disable, replacement, expiry, or reconnect.
pub(crate) async fn lock_resolved_binding(
    tx: &mut Transaction<'_, MySql>,
    user_id: &str,
    identity: &RunnerInferenceBindingIdentity,
) -> ServiceResult<ResolvedRunnerModelBinding> {
    let registry = sqlx::query(
        "SELECT inference_boot_nonce FROM edge_agent_registry
         WHERE user_id = ? AND edge_agent_id = ? AND inference_journal_id = ?
           AND registration_state = 1 AND inference_edge_id = edge_id
           AND last_heartbeat_at > DATE_SUB(NOW(6), INTERVAL 60 SECOND)
         FOR UPDATE",
    )
    .bind(user_id)
    .bind(identity.runner_id.as_str())
    .bind(identity.journal_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(storage)?
    .ok_or_else(|| ServiceError::not_found("Runner inference executor unavailable"))?;
    let row = sqlx::query(
        "SELECT enabled, definition_json, publication_revision FROM runner_model_binding_publications
         WHERE user_id = ? AND runner_id = ? AND journal_id = ? AND binding_id = ?
         ORDER BY publication_revision DESC LIMIT 1",
    ).bind(user_id).bind(identity.runner_id.as_str()).bind(identity.journal_id.as_str())
        .bind(identity.binding_id.as_str()).fetch_optional(&mut **tx).await.map_err(storage)?
        .ok_or_else(|| ServiceError::not_found("Runner model binding absent"))?;
    if !row.try_get::<bool, _>("enabled").map_err(storage)? {
        return Err(ServiceError::not_found("Runner model binding disabled"));
    }
    let definition: RunnerInferenceBindingDefinition = serde_json::from_str(
        &row.try_get::<String, _>("definition_json")
            .map_err(storage)?,
    )
    .map_err(|_| ServiceError::invalid("invalid durable Runner binding definition"))?;
    if &definition.identity != identity {
        return Err(ServiceError::conflict(
            "Runner model binding revision changed",
        ));
    }
    let boot: String = registry.try_get("inference_boot_nonce").map_err(storage)?;
    Ok(ResolvedRunnerModelBinding {
        user_id: user_id.to_owned(),
        definition,
        process_boot_nonce: RunnerInferenceId::new(boot).map_err(ServiceError::invalid)?,
        publication_revision: u64::try_from(
            row.try_get::<i64, _>("publication_revision")
                .map_err(storage)?,
        )
        .map_err(|_| ServiceError::invalid("invalid durable Runner publication revision"))?,
    })
}

pub async fn resolve_runner_model_binding(
    pool: &SharedPool,
    user_id: &str,
    identity: &RunnerInferenceBindingIdentity,
) -> ServiceResult<ResolvedRunnerModelBinding> {
    let mut tx = pool.get().begin().await.map_err(storage)?;
    let resolved = lock_resolved_binding(&mut tx, user_id, identity).await?;
    tx.commit().await.map_err(storage)?;
    Ok(resolved)
}

/// Bounded current inventory for the canonical effective Model Access catalog.
/// Historical receipts, un-enrolled connections, disabled revisions and other
/// owners never enter this projection. Admission still revalidates under lock.
pub async fn list_effective_runner_model_bindings(
    pool: &SharedPool,
    user_id: &str,
) -> ServiceResult<Vec<ResolvedRunnerModelBinding>> {
    let rows = sqlx::query(
        "SELECT p.definition_json, p.publication_revision, r.inference_boot_nonce
         FROM runner_model_binding_publications p
         JOIN edge_agent_registry r ON r.user_id = p.user_id AND r.edge_agent_id = p.runner_id
           AND r.inference_journal_id = p.journal_id
         WHERE p.user_id = ? AND p.enabled = TRUE AND r.registration_state = 1
           AND r.inference_edge_id = r.edge_id AND r.inference_boot_nonce IS NOT NULL
           AND r.last_heartbeat_at > DATE_SUB(NOW(6), INTERVAL 60 SECOND)
           AND NOT EXISTS (SELECT 1 FROM runner_model_binding_publications newer
             WHERE newer.user_id = p.user_id AND newer.runner_id = p.runner_id
               AND newer.journal_id = p.journal_id AND newer.binding_id = p.binding_id
               AND newer.publication_revision > p.publication_revision)
         ORDER BY p.runner_id, p.journal_id, p.binding_id LIMIT 257",
    )
    .bind(user_id)
    .fetch_all(pool.get())
    .await
    .map_err(storage)?;
    if rows.len() > 256 {
        return Err(ServiceError::invalid(
            "personal Runner catalog exceeds 256 bindings",
        ));
    }
    rows.into_iter()
        .map(|row| {
            let encoded: String = row.try_get("definition_json").map_err(storage)?;
            let definition = serde_json::from_str(&encoded)
                .map_err(|_| ServiceError::invalid("invalid durable Runner definition"))?;
            let boot: String = row.try_get("inference_boot_nonce").map_err(storage)?;
            let revision: i64 = row.try_get("publication_revision").map_err(storage)?;
            Ok(ResolvedRunnerModelBinding {
                user_id: user_id.into(),
                definition,
                process_boot_nonce: RunnerInferenceId::new(boot).map_err(ServiceError::invalid)?,
                publication_revision: u64::try_from(revision)
                    .map_err(|_| ServiceError::invalid("invalid durable publication revision"))?,
            })
        })
        .collect()
}

/// Latest enabled bindings, including a Runner whose live lease has expired.
/// This query is presentation-only; execution always goes through
/// `resolve_runner_offering`, which revalidates current authority under lock.
pub async fn list_runner_model_catalog_bindings(
    pool: &SharedPool,
    user_id: &str,
) -> ServiceResult<Vec<RunnerModelCatalogBinding>> {
    let rows = sqlx::query(
        "SELECT p.definition_json,
           IF(r.registration_state = 1 AND r.inference_edge_id = r.edge_id
              AND r.inference_boot_nonce IS NOT NULL
              AND r.last_heartbeat_at > DATE_SUB(NOW(6), INTERVAL 60 SECOND), 1, 0) AS online
         FROM runner_model_binding_publications p
         LEFT JOIN edge_agent_registry r
           ON r.user_id = p.user_id AND r.edge_agent_id = p.runner_id
          AND r.inference_journal_id = p.journal_id
         WHERE p.user_id = ? AND p.enabled = TRUE
           AND NOT EXISTS (SELECT 1 FROM runner_model_binding_publications newer
             WHERE newer.user_id = p.user_id AND newer.runner_id = p.runner_id
               AND newer.journal_id = p.journal_id AND newer.binding_id = p.binding_id
               AND newer.publication_revision > p.publication_revision)
         ORDER BY p.runner_id, p.journal_id, p.binding_id LIMIT 257",
    )
    .bind(user_id)
    .fetch_all(pool.get())
    .await
    .map_err(storage)?;
    if rows.len() > 256 {
        return Err(ServiceError::invalid(
            "personal Runner catalog exceeds 256 bindings",
        ));
    }
    rows.into_iter()
        .map(|row| {
            let encoded: String = row.try_get("definition_json").map_err(storage)?;
            let definition = serde_json::from_str(&encoded)
                .map_err(|_| ServiceError::invalid("invalid durable Runner definition"))?;
            Ok(RunnerModelCatalogBinding {
                user_id: user_id.into(),
                definition,
                online: row.try_get::<i64, _>("online").map_err(storage)? == 1,
            })
        })
        .collect()
}

/// Resolve the current revision of one stable Offering, never by display name
/// and never falling back to a Server catalog or provider gateway.
pub async fn resolve_runner_offering(
    pool: &SharedPool,
    user_id: &str,
    offering_id: &str,
) -> ServiceResult<ResolvedRunnerModelBinding> {
    let selected = list_effective_runner_model_bindings(pool, user_id)
        .await?
        .into_iter()
        .find(|binding| runner_offering_id(user_id, &binding.definition.identity) == offering_id)
        .ok_or_else(|| ServiceError::not_found("Runner Offering is unavailable"))?;
    resolve_runner_model_binding(pool, user_id, &selected.definition.identity).await
}
