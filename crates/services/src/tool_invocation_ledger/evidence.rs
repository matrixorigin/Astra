//! Coverage is witnessed by Run admission grants; row enumeration alone is not proof.
use super::*;
use astra_turn_types::ToolInvocationCompletionRef;
use std::collections::BTreeSet;

#[derive(Debug)]
pub struct ToolInvocationRunEvidence {
    pub history: crate::runs::VerifiedRunActionHistory,
    pub records: Vec<ToolInvocationRecord>,
    pub completions: Vec<ToolInvocationCompletionRef>,
}

impl DatabaseToolInvocationLedger {
    pub async fn inspect_run_evidence(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<ToolInvocationRunEvidence, ToolInvocationLedgerStoreError> {
        let mut tx = self.pool.get().begin().await?;
        let evidence =
            Self::inspect_run_evidence_in_transaction(&mut tx, user_id, session_id, run_id).await?;
        tx.commit().await?;
        Ok(evidence)
    }

    /// Read the journal, hot ledger and every archive in one consistent view.
    /// Assessment must keep dependent artifact reads in this same transaction.
    pub async fn inspect_run_evidence_in_transaction(
        tx: &mut Transaction<'_, MySql>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<ToolInvocationRunEvidence, ToolInvocationLedgerStoreError> {
        let history = crate::runs::load_verified_run_action_history_in_transaction(
            tx, user_id, session_id, run_id,
        )
        .await?;
        let rows = sqlx::query(
            "SELECT turn_chain_id, invocation_id, identity_key,
                    JSON_UNQUOTE(fingerprint_json) AS fingerprint_json,
                    JSON_UNQUOTE(decision_json) AS decision_json,
                    JSON_UNQUOTE(outcome_json) AS outcome_json,
                    JSON_UNQUOTE(completion_source_json) AS completion_source_json,
                    state, dispatch_certainty, attempt_count, dispatch_owner,
                    CAST(UNIX_TIMESTAMP(dispatch_lease_expires_at) * 1000 AS UNSIGNED)
                        AS dispatch_lease_expires_at_epoch_ms
             FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await?;
        let mut records = BTreeMap::new();
        for row in rows {
            let identity = ToolInvocationIdentity::new(
                user_id,
                session_id,
                run_id,
                row.try_get::<String, _>("turn_chain_id")?,
                row.try_get::<String, _>("invocation_id")?,
            )?;
            if row.try_get::<String, _>("identity_key")? != identity.storage_key() {
                return Err(integrity("hot identity key mismatch"));
            }
            insert_record(&mut records, decode_record(&row, &identity)?)?;
        }
        let archives = sqlx::query(
            "SELECT chunks.chunk_index, chunks.artifact_id, chunks.first_identity_key,
                    chunks.last_identity_key, chunks.record_count, chunks.encoded_bytes,
                    artifacts.status, artifacts.artifact_kind, artifacts.source,
                    artifacts.content_json, JSON_UNQUOTE(artifacts.metadata) AS metadata
             FROM tool_invocation_archive_chunks chunks
             LEFT JOIN session_artifacts artifacts
               ON artifacts.user_id = chunks.user_id AND artifacts.session_id = chunks.session_id
              AND artifacts.artifact_id = chunks.artifact_id
             WHERE chunks.user_id = ? AND chunks.session_id = ? AND chunks.run_id = ?
             ORDER BY chunks.chunk_index",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await?;
        for (index, row) in archives.iter().enumerate() {
            if row.try_get::<u64, _>("chunk_index")? != index as u64 + 1 {
                return Err(integrity("archive chunk sequence is incomplete"));
            }
            let chunk = validate_archive(row, user_id, session_id, run_id)?;
            for record in chunk.records {
                insert_record(&mut records, record)?;
            }
        }
        let admitted = history
            .grants
            .iter()
            .filter_map(|grant| grant.action_id.strip_prefix("tool_invocation:"))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let mut observed = BTreeSet::new();
        let mut completions = Vec::new();
        for (key, record) in &records {
            let cache = matches!(
                record.completion_source,
                Some(ToolInvocationCompletionSource::SemanticReadCache { .. })
            );
            let requires_grant =
                record.dispatch_certainty != DispatchCertainty::NotDispatched || cache;
            if requires_grant != admitted.contains(key) {
                return Err(integrity("invocation state and Run admission disagree"));
            }
            if !requires_grant {
                continue;
            }
            observed.insert(key.clone());
            if !record.state.is_terminal() || record.state == ToolInvocationState::OutcomeUnknown {
                return Err(ToolInvocationLedgerStoreError::EvidenceUnresolved(
                    key.clone(),
                ));
            }
            completions.push(ToolInvocationCompletionRef::from_record(record)?);
        }
        if admitted != observed {
            return Err(integrity(
                "admitted invocations are missing from the hot/archive ledger",
            ));
        }
        Ok(ToolInvocationRunEvidence {
            history,
            records: records.into_values().collect(),
            completions,
        })
    }
}

fn integrity(message: &str) -> ToolInvocationLedgerStoreError {
    ToolInvocationLedgerStoreError::EvidenceIntegrity(message.into())
}

fn insert_record(
    records: &mut BTreeMap<String, ToolInvocationRecord>,
    record: ToolInvocationRecord,
) -> Result<(), ToolInvocationLedgerStoreError> {
    record.validate()?;
    if records
        .insert(record.identity.storage_key(), record)
        .is_some()
    {
        return Err(integrity("duplicate hot/archive invocation"));
    }
    Ok(())
}

pub(super) fn validate_archive(
    row: &sqlx::mysql::MySqlRow,
    user_id: &str,
    session_id: &str,
    run_id: &str,
) -> Result<ToolInvocationArchiveChunk, ToolInvocationLedgerStoreError> {
    let artifact_id: String = row.try_get("artifact_id")?;
    let status: Option<String> = row.try_get("status")?;
    let content: Option<String> = row.try_get("content_json")?;
    let Some(content) = content.filter(|_| status.as_deref() == Some("active")) else {
        return Err(ToolInvocationLedgerStoreError::ArchiveUnavailable {
            artifact_id,
            status,
        });
    };
    if row
        .try_get::<Option<String>, _>("artifact_kind")?
        .as_deref()
        != Some("tool_invocation_archive_v1")
        || row.try_get::<Option<String>, _>("source")?.as_deref()
            != Some("invocation_ledger_compactor")
        || content.len() > TOOL_INVOCATION_ARCHIVE_CHUNK_MAX_BYTES
        || row.try_get::<u64, _>("encoded_bytes")? != content.len() as u64
    {
        return Err(integrity("archive provenance or size mismatch"));
    }
    let metadata_json = row
        .try_get::<Option<String>, _>("metadata")?
        .ok_or_else(|| integrity("archive metadata missing"))?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata_json).map_err(|source| {
        ToolInvocationLedgerStoreError::InvalidStoredJson {
            field: "archive_metadata",
            source,
        }
    })?;
    let hash = format!("sha256:{:x}", Sha256::digest(content.as_bytes()));
    if metadata["contentHash"].as_str() != Some(hash.as_str())
        || metadata["contractVersion"].as_str() != Some(TOOL_INVOCATION_ARCHIVE_VERSION)
        || metadata["encodedBytes"].as_u64() != Some(content.len() as u64)
    {
        return Err(integrity("archive hash or metadata mismatch"));
    }
    let chunk: ToolInvocationArchiveChunk = serde_json::from_str(&content).map_err(|source| {
        ToolInvocationLedgerStoreError::InvalidArchive {
            artifact_id: artifact_id.clone(),
            source,
        }
    })?;
    if chunk.version != TOOL_INVOCATION_ARCHIVE_VERSION
        || chunk.user_id != user_id
        || chunk.session_id != session_id
        || chunk.run_id != run_id
    {
        return Err(ToolInvocationLedgerStoreError::ArchiveScopeMismatch { artifact_id });
    }
    let count = chunk.records.len() as u64;
    if count == 0
        || row.try_get::<u64, _>("record_count")? != count
        || metadata["recordCount"].as_u64() != Some(count)
    {
        return Err(integrity("archive record count mismatch"));
    }
    let mut keys = Vec::with_capacity(chunk.records.len());
    for record in &chunk.records {
        record.validate()?;
        if !record.state.is_terminal() || record.state == ToolInvocationState::OutcomeUnknown {
            return Err(integrity("archive contains an unsettled invocation"));
        }
        if record.identity.user_id != user_id
            || record.identity.session_id != session_id
            || record.identity.run_id != run_id
        {
            return Err(integrity("archive record scope mismatch"));
        }
        keys.push(record.identity.storage_key());
    }
    if keys.windows(2).any(|pair| pair[0] >= pair[1])
        || keys.first() != Some(&row.try_get::<String, _>("first_identity_key")?)
        || keys.last() != Some(&row.try_get::<String, _>("last_identity_key")?)
    {
        return Err(integrity("archive identity bounds mismatch"));
    }
    Ok(chunk)
}
