//! Auditable inventory for the session-memory projection lifecycle.
//!
//! Recall is a relevance-ranked content operation, not a counting API. This
//! module derives exact extraction/version counts from authoritative journal
//! events and keeps the current logical snapshot count as a separate field.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::session_artifact_store::SessionArtifactStore;
use crate::session_journal::{JournalEvent, JournalEventType};

pub const SESSION_MEMORY_INVENTORY_SCHEMA_VERSION: u16 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMemoryInventory {
    pub schema_version: u16,
    /// Self-describing report identity. This report audits extraction
    /// lifecycle events; it is not a catalog of stored memory records.
    pub report_type: String,
    pub scope: String,
    pub contains_memory_identities: bool,
    pub session_id: String,
    /// Audit events, including skips and errors. This is not a memory count.
    pub extraction_events: u64,
    /// Successful snapshot writes. Multiple versions may update one logical
    /// active snapshot and therefore must not be described as memory count.
    pub successful_extraction_versions: u64,
    pub llm_versions: u64,
    pub rule_fallback_versions: u64,
    pub errored_attempts: u64,
    pub skipped_attempts: u64,
    pub distinct_successful_turns: u64,
    /// Turns with more than one successful write.
    pub duplicate_successful_turns: Vec<u32>,
    pub last_successful_turn: Option<u32>,
    pub last_outcome: Option<String>,
    pub last_reason: Option<String>,
    pub last_source: Option<String>,
    /// Exact for local inventory; `None` means this source cannot inspect the
    /// current snapshot store authoritatively.
    pub logical_current_snapshot_count: Option<u64>,
    pub inventory_source: String,
    #[serde(skip)]
    successful_turn_counts: BTreeMap<u32, u64>,
}

impl SessionMemoryInventory {
    pub fn empty(session_id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            schema_version: SESSION_MEMORY_INVENTORY_SCHEMA_VERSION,
            report_type: "session_memory_extraction_audit".to_string(),
            scope: "session".to_string(),
            contains_memory_identities: false,
            session_id: session_id.into(),
            extraction_events: 0,
            successful_extraction_versions: 0,
            llm_versions: 0,
            rule_fallback_versions: 0,
            errored_attempts: 0,
            skipped_attempts: 0,
            distinct_successful_turns: 0,
            duplicate_successful_turns: Vec::new(),
            last_successful_turn: None,
            last_outcome: None,
            last_reason: None,
            last_source: None,
            logical_current_snapshot_count: None,
            inventory_source: source.into(),
            successful_turn_counts: BTreeMap::new(),
        }
    }

    pub fn observe_extraction(&mut self, turn: Option<u32>, metadata: &serde_json::Value) {
        let Some(outcome) = metadata.get("outcome").and_then(serde_json::Value::as_str) else {
            return;
        };
        self.extraction_events = self.extraction_events.saturating_add(1);
        self.last_outcome = Some(outcome.to_string());
        self.last_reason = metadata
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        self.last_source = metadata
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        match outcome {
            "extracted" => {
                self.successful_extraction_versions =
                    self.successful_extraction_versions.saturating_add(1);
                match self.last_source.as_deref() {
                    Some("llm") => self.llm_versions = self.llm_versions.saturating_add(1),
                    Some("rule_fallback") => {
                        self.rule_fallback_versions = self.rule_fallback_versions.saturating_add(1)
                    }
                    _ => {}
                }
                if let Some(turn) = turn {
                    *self.successful_turn_counts.entry(turn).or_default() += 1;
                    self.last_successful_turn = Some(
                        self.last_successful_turn
                            .map_or(turn, |last| last.max(turn)),
                    );
                }
            }
            "errored" => self.errored_attempts = self.errored_attempts.saturating_add(1),
            "skipped" => self.skipped_attempts = self.skipped_attempts.saturating_add(1),
            _ => {}
        }
        self.refresh_turn_summary();
    }

    fn refresh_turn_summary(&mut self) {
        self.distinct_successful_turns = self.successful_turn_counts.len() as u64;
        self.duplicate_successful_turns = self
            .successful_turn_counts
            .iter()
            .filter_map(|(turn, count)| (*count > 1).then_some(*turn))
            .collect();
    }
}

pub fn inventory_from_journal_events(
    session_id: &str,
    events: &[JournalEvent],
    source: &str,
) -> SessionMemoryInventory {
    let mut inventory = SessionMemoryInventory::empty(session_id, source);
    for event in events {
        if event.event_type != JournalEventType::SessionMemoryExtraction {
            continue;
        }
        let metadata = event.metadata.as_ref().unwrap_or(&serde_json::Value::Null);
        inventory.observe_extraction(event.turn, metadata);
    }
    inventory
}

pub fn load_local_session_memory_inventory(
    session_id: &str,
) -> std::io::Result<SessionMemoryInventory> {
    let events = match crate::session_journal::read_journal_for_digest(session_id) {
        Ok((events, non_empty_lines, malformed_lines)) => {
            if malformed_lines != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "session memory inventory cannot be exact: {malformed_lines} of {non_empty_lines} journal lines are malformed"
                    ),
                ));
            }
            events
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let mut inventory = inventory_from_journal_events(session_id, &events, "local_journal");
    let snapshot_path = crate::local_session_artifact_store()
        .session_path(session_id, "session-memory.md")
        .map_err(std::io::Error::other)?;
    inventory.logical_current_snapshot_count =
        Some(match std::fs::read_to_string(&snapshot_path) {
            Ok(content) => u64::from(!content.trim().is_empty()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!(
                        "read current session memory snapshot {}: {error}",
                        snapshot_path.display()
                    ),
                ));
            }
        });
    Ok(inventory)
}

pub async fn load_database_session_memory_inventory(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) -> Result<SessionMemoryInventory, String> {
    if !crate::storage::agent_session_exists_for_user(pool, session_id, user_id)
        .await
        .map_err(|error| format!("session memory inventory ownership check failed: {error}"))?
    {
        return Err("session memory inventory session is not owned by the user".to_string());
    }
    let rows = sqlx::query(
        "SELECT turn_seq, CAST(metadata AS CHAR) AS metadata_json \
         FROM agent_events \
         WHERE user_id = ? AND session_id = ? AND event_type = 'session_memory_extraction' \
         ORDER BY created_at ASC, event_id ASC",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load session memory inventory: {error}"))?;

    let mut inventory = SessionMemoryInventory::empty(session_id, "cloud_events");
    for row in rows {
        let turn = row
            .try_get::<Option<i64>, _>("turn_seq")
            .map_err(|error| format!("decode session memory inventory turn_seq: {error}"))?
            .map(|turn| {
                u32::try_from(turn).map_err(|_| {
                    format!("session memory inventory turn_seq is outside u32 range: {turn}")
                })
            })
            .transpose()?;
        let metadata_raw = row
            .try_get::<Option<String>, _>("metadata_json")
            .map_err(|error| format!("decode session memory inventory metadata: {error}"))?
            .ok_or_else(|| "session memory extraction event is missing metadata".to_string())?;
        let metadata = serde_json::from_str::<serde_json::Value>(&metadata_raw)
            .map_err(|error| format!("parse session memory inventory metadata: {error}"))?;
        inventory.observe_extraction(turn, &metadata);
    }
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::{
        SessionMemoryExtractionBreadcrumbs, SessionMemoryExtractionOutcome,
        SessionMemoryExtractionSkipReason, SessionMemoryExtractionSource,
    };

    #[test]
    fn inventory_distinguishes_versions_snapshot_and_duplicate_turns() {
        let breadcrumbs = SessionMemoryExtractionBreadcrumbs::default();
        let events = vec![
            JournalEvent::session_memory_extraction(
                Some("session-1"),
                2,
                10,
                SessionMemoryExtractionOutcome::Extracted {
                    source: SessionMemoryExtractionSource::Llm,
                    bytes_written: 100,
                },
                &breadcrumbs,
            ),
            JournalEvent::session_memory_extraction(
                Some("session-1"),
                2,
                5,
                SessionMemoryExtractionOutcome::Extracted {
                    source: SessionMemoryExtractionSource::RuleFallback,
                    bytes_written: 90,
                },
                &breadcrumbs,
            ),
            JournalEvent::session_memory_extraction(
                Some("session-1"),
                3,
                0,
                SessionMemoryExtractionOutcome::Skipped {
                    reason: SessionMemoryExtractionSkipReason::AlreadyCurrent,
                },
                &breadcrumbs,
            ),
        ];

        let inventory = inventory_from_journal_events("session-1", &events, "test");
        assert_eq!(inventory.successful_extraction_versions, 2);
        assert_eq!(inventory.distinct_successful_turns, 1);
        assert_eq!(inventory.duplicate_successful_turns, vec![2]);
        assert_eq!(inventory.llm_versions, 1);
        assert_eq!(inventory.rule_fallback_versions, 1);
        assert_eq!(inventory.skipped_attempts, 1);
        assert_eq!(inventory.last_outcome.as_deref(), Some("skipped"));
        assert_eq!(inventory.last_reason.as_deref(), Some("already_current"));
    }

    #[test]
    fn local_inventory_reports_exact_snapshot_and_version_dimensions() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "inventory-local";
        let breadcrumbs = SessionMemoryExtractionBreadcrumbs::default();
        let writer = crate::session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&JournalEvent::session_memory_extraction(
                Some(session_id),
                4,
                12,
                SessionMemoryExtractionOutcome::Extracted {
                    source: SessionMemoryExtractionSource::Llm,
                    bytes_written: 120,
                },
                &breadcrumbs,
            ))
            .unwrap();

        let snapshot_path = crate::local_session_artifact_store()
            .session_path(session_id, "session-memory.md")
            .unwrap();
        std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        std::fs::write(
            &snapshot_path,
            "# Session Memory\n\n## Current State\n- active",
        )
        .unwrap();

        let inventory = load_local_session_memory_inventory(session_id).unwrap();
        assert_eq!(inventory.report_type, "session_memory_extraction_audit");
        assert_eq!(inventory.scope, "session");
        assert!(!inventory.contains_memory_identities);
        assert_eq!(inventory.successful_extraction_versions, 1);
        assert_eq!(inventory.logical_current_snapshot_count, Some(1));
        assert_eq!(inventory.inventory_source, "local_journal");
    }

    #[test]
    fn local_inventory_reports_empty_only_when_artifacts_are_absent() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session_journal::JournalDirGuard::new(temp.path());

        let inventory = load_local_session_memory_inventory("inventory-absent").unwrap();

        assert_eq!(inventory.extraction_events, 0);
        assert_eq!(inventory.successful_extraction_versions, 0);
        assert_eq!(inventory.logical_current_snapshot_count, Some(0));
        assert_eq!(inventory.inventory_source, "local_journal");
    }

    #[test]
    fn local_inventory_fails_closed_when_journal_integrity_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "inventory-malformed";
        let path = crate::session_journal::journal_file_path(session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{not-json}\n").unwrap();

        let error = load_local_session_memory_inventory(session_id).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("cannot be exact"));
    }

    #[test]
    fn local_inventory_does_not_report_zero_when_snapshot_is_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "inventory-unreadable-snapshot";
        let snapshot_path = crate::local_session_artifact_store()
            .session_path(session_id, "session-memory.md")
            .unwrap();
        std::fs::create_dir_all(&snapshot_path).unwrap();

        let error = load_local_session_memory_inventory(session_id).unwrap_err();

        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            error
                .to_string()
                .contains("read current session memory snapshot")
        );
    }
}
