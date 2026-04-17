//! Domain adapters bridging runtime pipeline learning modules with the unified sync engine.
//!
//! Lives in `astra-runtime` because `DomainAdapter` / `CloudTransport` are defined in
//! `astra-services` while `EntityGraph`, `PatternLibrary`, and persistence live here.
//! Adding this module to `services` would require `services` → `runtime`, which cycles with
//! `runtime` → `services`.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use astra_evolution::persistence::{self, LearningSnapshot, ToolHealthEntry};
use astra_pipeline::calibration::ProgressiveCalibrator;
use astra_pipeline::entity::EntityGraph;
use astra_pipeline::pattern::PatternLibrary;
use astra_text_utils::str_preview::prefix_chars;

use astra_services::event_ingestion;
use astra_services::state_sync::{PlanTemplateSyncRow, StateSyncService};
use astra_services::sync_engine::sha256_checksum;
use astra_services::{
    CloudTransport, DomainAdapter, MergeResult, PayloadFormat, PullResult, PushResult, SyncDomain,
    SyncEnvelope, SyncError, SyncPayload, TaskLeaseHoldCache, TaskRecord,
};

fn payload_sync_version(data: &[u8]) -> u64 {
    let h = sha256_checksum(data);
    let prefix = prefix_chars(&h, 16);
    u64::from_str_radix(&prefix, 16).unwrap_or(1)
}

/// Wire format for [`SyncDomain::Tasks`] push; pull uses a plain JSON array of [`TaskRecord`].
#[derive(serde::Serialize, serde::Deserialize)]
struct TaskSyncPackBody {
    holder_agent_id: String,
    tasks: Vec<TaskRecord>,
}

// ─── Learning Adapter ──────────────────────────────────────────────────────────

/// Bridges the runtime learning modules (EntityGraph, PatternLibrary, Calibrator, ToolHealth)
/// with the unified sync engine's `DomainAdapter` trait.
pub struct LearningAdapter {
    entity_graph: Arc<Mutex<EntityGraph>>,
    pattern_library: Arc<Mutex<PatternLibrary>>,
    calibrator: Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: Arc<Mutex<Vec<ToolHealthEntry>>>,
    envelope: Arc<Mutex<SyncEnvelope>>,
}

impl LearningAdapter {
    pub fn new(
        entity_graph: Arc<Mutex<EntityGraph>>,
        pattern_library: Arc<Mutex<PatternLibrary>>,
        calibrator: Arc<Mutex<ProgressiveCalibrator>>,
        tool_health: Arc<Mutex<Vec<ToolHealthEntry>>>,
    ) -> Self {
        Self {
            entity_graph,
            pattern_library,
            calibrator,
            tool_health,
            envelope: Arc::new(Mutex::new(SyncEnvelope::new(SyncDomain::Learning))),
        }
    }

    fn count_items(snapshot: &LearningSnapshot) -> u32 {
        snapshot.entities.len() as u32
            + snapshot.patterns.len() as u32
            + if snapshot.calibration.is_some() { 1 } else { 0 }
            + snapshot.tool_health.len() as u32
    }
}

#[async_trait]
impl DomainAdapter for LearningAdapter {
    fn domain(&self) -> SyncDomain {
        SyncDomain::Learning
    }

    fn export_full(&self) -> Result<SyncPayload, SyncError> {
        let health = self.tool_health.lock().map_err(|e| {
            SyncError::permanent(SyncDomain::Learning, format!("lock tool_health: {e}"))
        })?;
        let snapshot = persistence::export_from_modules_with_health(
            &self.entity_graph,
            &self.pattern_library,
            &self.calibrator,
            &health,
        );
        let item_count = Self::count_items(&snapshot);
        let data = serde_json::to_vec(&snapshot).map_err(|e| {
            SyncError::permanent(SyncDomain::Learning, format!("serialize snapshot: {e}"))
        })?;
        let checksum = sha256_checksum(&data);
        Ok(SyncPayload {
            data,
            format: PayloadFormat::Full,
            checksum,
            item_count,
            compressed: false,
        })
    }

    fn export_delta(&self) -> Result<Option<SyncPayload>, SyncError> {
        if !persistence::has_dirty_learning_data(
            &self.entity_graph,
            &self.pattern_library,
            &self.calibrator,
        ) {
            return Ok(None);
        }

        let delta = persistence::export_dirty_learning_from_modules(
            &self.entity_graph,
            &self.pattern_library,
            &self.calibrator,
        );

        match delta {
            Some(d) => {
                let item_count = d.delta_count;
                let baseline = d.baseline_epoch;
                let data = serde_json::to_vec(&d).map_err(|e| {
                    SyncError::permanent(SyncDomain::Learning, format!("serialize delta: {e}"))
                })?;
                let checksum = sha256_checksum(&data);
                Ok(Some(SyncPayload {
                    data,
                    format: PayloadFormat::Delta {
                        baseline_version: baseline,
                    },
                    checksum,
                    item_count,
                    compressed: false,
                }))
            }
            None => Ok(None),
        }
    }

    fn merge_remote(&self, remote: &SyncPayload) -> Result<MergeResult, SyncError> {
        let snapshot: LearningSnapshot = serde_json::from_slice(&remote.data).map_err(|e| {
            SyncError::permanent(
                SyncDomain::Learning,
                format!("deserialize remote snapshot: {e}"),
            )
        })?;

        let before_entities = self.entity_graph.lock().map(|g| g.len()).unwrap_or(0);
        let before_patterns = self.pattern_library.lock().map(|l| l.len()).unwrap_or(0);

        persistence::merge_into_modules(
            &snapshot,
            &self.entity_graph,
            &self.pattern_library,
            &self.calibrator,
        );

        // Merge tool health entries (most-recent-updated wins)
        if !snapshot.tool_health.is_empty()
            && let Ok(mut local_health) = self.tool_health.lock()
        {
            for remote_entry in &snapshot.tool_health {
                if let Some(local) = local_health
                    .iter_mut()
                    .find(|h| h.name == remote_entry.name)
                {
                    if remote_entry.last_updated_epoch > local.last_updated_epoch {
                        *local = remote_entry.clone();
                    }
                } else {
                    local_health.push(remote_entry.clone());
                }
            }
        }

        let after_entities = self.entity_graph.lock().map(|g| g.len()).unwrap_or(0);
        let after_patterns = self.pattern_library.lock().map(|l| l.len()).unwrap_or(0);

        let added = (after_entities.saturating_sub(before_entities)
            + after_patterns.saturating_sub(before_patterns)) as u32;
        // The merge strategy updates existing entries in-place; we approximate
        let updated = snapshot.entities.len() as u32 + snapshot.patterns.len() as u32;

        Ok(MergeResult {
            items_added: added,
            items_updated: updated.saturating_sub(added),
            items_removed: 0,
            conflicts_auto_resolved: 0,
        })
    }

    fn resolve_conflict(
        &self,
        local: &SyncPayload,
        remote: &SyncPayload,
    ) -> Result<SyncPayload, SyncError> {
        // Strategy: merge both — deserialize both, union local items not in remote
        let mut local_snap: LearningSnapshot = serde_json::from_slice(&local.data)
            .map_err(|e| SyncError::permanent(SyncDomain::Learning, format!("deser local: {e}")))?;
        let remote_snap: LearningSnapshot = serde_json::from_slice(&remote.data).map_err(|e| {
            SyncError::permanent(SyncDomain::Learning, format!("deser remote: {e}"))
        })?;

        // Merge remote entities into local (remote wins on duplicate by recency)
        let remote_entity_names: HashSet<String> = remote_snap
            .entities
            .iter()
            .map(|e| e.name.clone())
            .collect();
        for local_ent in &local_snap.entities {
            if !remote_entity_names.contains(&local_ent.name) {
                // Local-only entity; keep it
            }
        }
        // Start from remote, add local-only items
        let mut merged_entities = remote_snap.entities.clone();
        for local_ent in &local_snap.entities {
            if !remote_entity_names.contains(&local_ent.name) {
                merged_entities.push(local_ent.clone());
            }
        }

        let remote_pattern_sigs: HashSet<String> = remote_snap
            .patterns
            .iter()
            .map(|p| p.signature.clone())
            .collect();
        let mut merged_patterns = remote_snap.patterns.clone();
        for local_pat in &local_snap.patterns {
            if !remote_pattern_sigs.contains(&local_pat.signature) {
                merged_patterns.push(local_pat.clone());
            }
        }

        // Tool health: most-recently-updated wins per tool
        let mut health_map: std::collections::HashMap<String, ToolHealthEntry> = remote_snap
            .tool_health
            .into_iter()
            .map(|h| (h.name.clone(), h))
            .collect();
        for local_h in local_snap.tool_health.drain(..) {
            let entry = health_map
                .entry(local_h.name.clone())
                .or_insert(local_h.clone());
            if local_h.last_updated_epoch > entry.last_updated_epoch {
                *entry = local_h;
            }
        }

        let merged = LearningSnapshot {
            version: 1,
            snapshot_epoch: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            entities: merged_entities,
            patterns: merged_patterns,
            calibration: remote_snap.calibration.or(local_snap.calibration),
            tool_health: health_map.into_values().collect(),
            // Active canaries are runtime-local rollback state. Keep them in the
            // local learning snapshot for restart recovery, but never merge or
            // sync them across devices.
            active_canary: None,
        };

        let item_count = Self::count_items(&merged);
        let data = serde_json::to_vec(&merged).map_err(|e| {
            SyncError::permanent(SyncDomain::Learning, format!("serialize merged: {e}"))
        })?;
        let checksum = sha256_checksum(&data);
        Ok(SyncPayload {
            data,
            format: PayloadFormat::Full,
            checksum,
            item_count,
            compressed: false,
        })
    }

    fn validate(&self, payload: &SyncPayload) -> Result<(), SyncError> {
        // Verify checksum
        let computed = sha256_checksum(&payload.data);
        if computed != payload.checksum {
            return Err(SyncError::permanent(
                SyncDomain::Learning,
                format!(
                    "checksum mismatch: expected {}, got {}",
                    payload.checksum, computed
                ),
            ));
        }
        // Verify deserialization succeeds
        let _: LearningSnapshot = serde_json::from_slice(&payload.data).map_err(|e| {
            SyncError::permanent(SyncDomain::Learning, format!("invalid payload: {e}"))
        })?;
        Ok(())
    }

    fn envelope(&self) -> SyncEnvelope {
        self.envelope
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|_| SyncEnvelope::new(SyncDomain::Learning))
    }

    fn set_envelope(&self, envelope: SyncEnvelope) {
        if let Ok(mut e) = self.envelope.lock() {
            *e = envelope;
        }
    }

    fn has_dirty_data(&self) -> bool {
        // Check envelope dirty state
        if self.envelope().sync_state.is_dirty() {
            return true;
        }
        // Also check module-level dirty flags
        persistence::has_dirty_learning_data(
            &self.entity_graph,
            &self.pattern_library,
            &self.calibrator,
        )
    }

    fn estimated_size(&self) -> usize {
        // Rough estimate: each entity ~500 bytes, each pattern ~300 bytes
        let entities = self.entity_graph.lock().map(|g| g.len()).unwrap_or(0);
        let patterns = self.pattern_library.lock().map(|l| l.len()).unwrap_or(0);
        entities * 500 + patterns * 300 + 2048 // calibration + tool_health overhead
    }

    fn clear_dirty(&self) -> Result<(), SyncError> {
        persistence::clear_dirty_learning_in_modules(
            &self.entity_graph,
            &self.pattern_library,
            &self.calibrator,
        );
        Ok(())
    }
}

// ─── MatrixOne Transport ───────────────────────────────────────────────────────

/// Cloud transport wrapping `StateSyncService` for the Learning domain.
///
/// Bridges the new unified sync engine protocol with the existing
/// `MatrixOneSyncService` versioned push/pull methods.
pub struct MatrixOneTransport {
    sync_service: Arc<dyn StateSyncService>,
    profile: String,
    /// Must match [`TaskSyncPackBody::holder_agent_id`] for [`SyncDomain::Tasks`] pushes.
    tasks_holder_agent_id: String,
}

impl MatrixOneTransport {
    pub fn new(
        sync_service: Arc<dyn StateSyncService>,
        profile: impl Into<String>,
        tasks_holder_agent_id: impl Into<String>,
    ) -> Self {
        Self {
            sync_service,
            profile: profile.into(),
            tasks_holder_agent_id: tasks_holder_agent_id.into(),
        }
    }
}

#[async_trait]
impl CloudTransport for MatrixOneTransport {
    async fn push(
        &self,
        user_id: &str,
        domain: SyncDomain,
        payload: &SyncPayload,
        expected_version: Option<u64>,
    ) -> Result<PushResult, SyncError> {
        match domain {
            SyncDomain::Learning => {
                // Convert SyncPayload bytes to JSON string
                let json_str = String::from_utf8(payload.data.clone()).map_err(|e| {
                    SyncError::permanent(SyncDomain::Learning, format!("invalid UTF-8: {e}"))
                })?;

                // Deserialize to count entities/patterns for the push API
                let snapshot: LearningSnapshot = serde_json::from_str(&json_str).map_err(|e| {
                    SyncError::permanent(SyncDomain::Learning, format!("deserialize for push: {e}"))
                })?;

                let entity_count = snapshot.entities.len() as u32;
                let pattern_count = snapshot.patterns.len() as u32;
                let has_calibration = snapshot.calibration.is_some();

                // Convert u64 → Option<i64> for the versioned API
                let expected_i64 = expected_version.map(|v| v as i64);

                let result = self
                    .sync_service
                    .push_learning_versioned(
                        user_id,
                        &self.profile,
                        &json_str,
                        entity_count,
                        pattern_count,
                        has_calibration,
                        expected_i64,
                    )
                    .await;

                if result.success {
                    Ok(PushResult {
                        success: true,
                        new_version: result.new_version.map(|v| v as u64),
                        is_conflict: false,
                        remote_payload: None,
                        message: "ok".to_string(),
                    })
                } else if result.is_conflict {
                    // On conflict, pull the remote payload so the orchestrator can resolve
                    let remote_payload = self.pull(user_id, SyncDomain::Learning).await?.payload;
                    Ok(PushResult {
                        success: false,
                        new_version: None,
                        is_conflict: true,
                        remote_payload,
                        message: result.message,
                    })
                } else {
                    Err(SyncError::transient(SyncDomain::Learning, result.message))
                }
            }
            SyncDomain::Preferences => {
                let map: BTreeMap<String, String> =
                    serde_json::from_slice(&payload.data).map_err(|e| {
                        SyncError::permanent(
                            SyncDomain::Preferences,
                            format!("deserialize preferences pack: {e}"),
                        )
                    })?;
                for (k, v) in map {
                    let r = self.sync_service.push_preference(user_id, &k, &v).await;
                    if !r.success {
                        return Err(SyncError::transient(SyncDomain::Preferences, r.message));
                    }
                }
                let ver = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(PushResult {
                    success: true,
                    new_version: Some(ver),
                    is_conflict: false,
                    remote_payload: None,
                    message: "ok".to_string(),
                })
            }
            SyncDomain::Templates => Err(SyncError::permanent(
                SyncDomain::Templates,
                "templates are cloud-sourced; edge does not push template packs",
            )),
            SyncDomain::Tasks => {
                let body: TaskSyncPackBody =
                    serde_json::from_slice(&payload.data).map_err(|e| {
                        SyncError::permanent(
                            SyncDomain::Tasks,
                            format!("deserialize tasks pack: {e}"),
                        )
                    })?;
                if body.holder_agent_id != self.tasks_holder_agent_id {
                    return Err(SyncError::permanent(
                        SyncDomain::Tasks,
                        "holder_agent_id does not match this transport",
                    ));
                }
                let inner = serde_json::to_string(&body.tasks).map_err(|e| {
                    SyncError::permanent(SyncDomain::Tasks, format!("serialize tasks: {e}"))
                })?;
                self.sync_service
                    .push_tasks_pack_held(user_id, &body.holder_agent_id, &inner)
                    .await
                    .map_err(|e| SyncError::transient(SyncDomain::Tasks, e))?;
                let ver = payload_sync_version(&payload.data);
                Ok(PushResult {
                    success: true,
                    new_version: Some(ver),
                    is_conflict: false,
                    remote_payload: None,
                    message: "ok".to_string(),
                })
            }
            other => Err(SyncError::permanent(
                other,
                format!("MatrixOneTransport does not support domain {other:?}"),
            )),
        }
    }

    async fn pull(&self, user_id: &str, domain: SyncDomain) -> Result<PullResult, SyncError> {
        match domain {
            SyncDomain::Learning => {
                let versioned = self
                    .sync_service
                    .pull_learning_versioned(user_id, &self.profile)
                    .await
                    .map_err(|e| SyncError::transient(SyncDomain::Learning, e))?;

                match versioned {
                    Some(vs) => {
                        let data = vs.json.into_bytes();
                        let checksum = sha256_checksum(&data);
                        // Count items for the payload metadata
                        let item_count = serde_json::from_slice::<LearningSnapshot>(&data)
                            .map(|s| LearningAdapter::count_items(&s))
                            .unwrap_or(0);

                        Ok(PullResult {
                            payload: Some(SyncPayload {
                                data,
                                format: PayloadFormat::Full,
                                checksum,
                                item_count,
                                compressed: false,
                            }),
                            version: Some(vs.version as u64),
                            message: "ok".to_string(),
                        })
                    }
                    None => Ok(PullResult {
                        payload: None,
                        version: None,
                        message: "no remote snapshot".to_string(),
                    }),
                }
            }
            SyncDomain::Preferences => {
                let rows = self
                    .sync_service
                    .pull_all_preferences(user_id)
                    .await
                    .map_err(|e| SyncError::transient(SyncDomain::Preferences, e))?;
                let map: BTreeMap<String, String> = rows.into_iter().collect();
                let item_count = map.len() as u32;
                let data = serde_json::to_vec(&map).map_err(|e| {
                    SyncError::permanent(
                        SyncDomain::Preferences,
                        format!("serialize preferences: {e}"),
                    )
                })?;
                let checksum = sha256_checksum(&data);
                let version = payload_sync_version(&data);
                Ok(PullResult {
                    payload: Some(SyncPayload {
                        data,
                        format: PayloadFormat::Full,
                        checksum,
                        item_count,
                        compressed: false,
                    }),
                    version: Some(version),
                    message: "ok".to_string(),
                })
            }
            SyncDomain::Templates => {
                let json = self
                    .sync_service
                    .pull_plan_templates_pack(user_id)
                    .await
                    .map_err(|e| SyncError::transient(SyncDomain::Templates, e))?;
                let data = json.into_bytes();
                let checksum = sha256_checksum(&data);
                let item_count = serde_json::from_slice::<serde_json::Value>(&data)
                    .ok()
                    .and_then(|v| v.as_array().map(|a| a.len() as u32))
                    .unwrap_or(0);
                let version = payload_sync_version(&data);
                Ok(PullResult {
                    payload: Some(SyncPayload {
                        data,
                        format: PayloadFormat::Full,
                        checksum,
                        item_count,
                        compressed: false,
                    }),
                    version: Some(version),
                    message: "ok".to_string(),
                })
            }
            SyncDomain::Tasks => {
                let json = self
                    .sync_service
                    .pull_tasks_pack(user_id)
                    .await
                    .map_err(|e| SyncError::transient(SyncDomain::Tasks, e))?;
                let data = json.into_bytes();
                let checksum = sha256_checksum(&data);
                let item_count = serde_json::from_slice::<Vec<TaskRecord>>(&data)
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);
                let version = payload_sync_version(&data);
                Ok(PullResult {
                    payload: Some(SyncPayload {
                        data,
                        format: PayloadFormat::Full,
                        checksum,
                        item_count,
                        compressed: false,
                    }),
                    version: Some(version),
                    message: "ok".to_string(),
                })
            }
            other => Err(SyncError::permanent(
                other,
                format!("MatrixOneTransport does not support domain {other:?}"),
            )),
        }
    }

    async fn health_check(&self) -> bool {
        // Probe by attempting a lightweight pull; if it doesn't error, cloud is up
        self.sync_service
            .pull_learning_versioned("__health_check__", "__probe__")
            .await
            .is_ok()
    }
}

// ─── Event Adapter ─────────────────────────────────────────────────────────────

/// Adapter for the Events domain — write-only (pushed via ingestion, never pulled).
///
/// Shares the same [`event_ingestion::IngestionSender`] as
/// [`astra_runtime::matrix_cloud_runtime::MatrixCloudRuntime::enqueue_journal_events`].
pub struct EventAdapter {
    sender: event_ingestion::IngestionSender,
    envelope: Arc<Mutex<SyncEnvelope>>,
}

impl EventAdapter {
    pub fn new(sender: event_ingestion::IngestionSender) -> Self {
        Self {
            sender,
            envelope: Arc::new(Mutex::new(SyncEnvelope::new(SyncDomain::Events))),
        }
    }

    pub fn ingestion_sender(&self) -> &event_ingestion::IngestionSender {
        &self.sender
    }
}

#[async_trait]
impl DomainAdapter for EventAdapter {
    fn domain(&self) -> SyncDomain {
        SyncDomain::Events
    }

    fn export_full(&self) -> Result<SyncPayload, SyncError> {
        Err(SyncError::permanent(
            SyncDomain::Events,
            "events do not support full export — they use their own batching mechanism",
        ))
    }

    fn export_delta(&self) -> Result<Option<SyncPayload>, SyncError> {
        Ok(None)
    }

    fn merge_remote(&self, _remote: &SyncPayload) -> Result<MergeResult, SyncError> {
        // Events are write-only; never pulled
        Ok(MergeResult::default())
    }

    fn resolve_conflict(
        &self,
        _local: &SyncPayload,
        _remote: &SyncPayload,
    ) -> Result<SyncPayload, SyncError> {
        Err(SyncError::permanent(
            SyncDomain::Events,
            "events are write-only — conflicts cannot occur",
        ))
    }

    fn validate(&self, _payload: &SyncPayload) -> Result<(), SyncError> {
        Ok(())
    }

    fn envelope(&self) -> SyncEnvelope {
        self.envelope
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|_| SyncEnvelope::new(SyncDomain::Events))
    }

    fn set_envelope(&self, envelope: SyncEnvelope) {
        if let Ok(mut e) = self.envelope.lock() {
            *e = envelope;
        }
    }

    fn has_dirty_data(&self) -> bool {
        // Events always use their own batching; never dirty from sync engine's perspective
        false
    }

    fn estimated_size(&self) -> usize {
        0
    }

    fn clear_dirty(&self) -> Result<(), SyncError> {
        Ok(())
    }
}

// ─── Template Adapter (pull-only cache) ───────────────────────────────────────

/// Cloud → edge cache of [`PlanTemplateSyncRow`]. Populated by `pull_domain(Templates)`;
/// edge never pushes template packs ([`SyncPolicy::templates`] uses [`astra_services::sync_engine::PushTrigger::Never`]).
///
/// Diagnostics: set environment variable `MO_SYNC_DEBUG=1` to print one line to stderr whenever
/// [`DomainAdapter::merge_remote`] replaces the template cache after a successful pull.
pub struct TemplateAdapter {
    cache: Arc<Mutex<Vec<PlanTemplateSyncRow>>>,
    envelope: Arc<Mutex<SyncEnvelope>>,
}

impl TemplateAdapter {
    pub fn new(cache: Arc<Mutex<Vec<PlanTemplateSyncRow>>>) -> Self {
        Self {
            cache,
            envelope: Arc::new(Mutex::new(SyncEnvelope::new(SyncDomain::Templates))),
        }
    }

    pub fn cache(&self) -> Arc<Mutex<Vec<PlanTemplateSyncRow>>> {
        Arc::clone(&self.cache)
    }
}

#[async_trait]
impl DomainAdapter for TemplateAdapter {
    fn domain(&self) -> SyncDomain {
        SyncDomain::Templates
    }

    fn export_full(&self) -> Result<SyncPayload, SyncError> {
        Err(SyncError::permanent(
            SyncDomain::Templates,
            "templates domain is pull-only — no edge push payload",
        ))
    }

    fn export_delta(&self) -> Result<Option<SyncPayload>, SyncError> {
        Ok(None)
    }

    fn merge_remote(&self, remote: &SyncPayload) -> Result<MergeResult, SyncError> {
        let rows: Vec<PlanTemplateSyncRow> = serde_json::from_slice(&remote.data).map_err(|e| {
            SyncError::permanent(
                SyncDomain::Templates,
                format!("deserialize template pack: {e}"),
            )
        })?;
        let n = rows.len() as u32;
        if let Ok(mut g) = self.cache.lock() {
            *g = rows;
        }
        if std::env::var("MO_SYNC_DEBUG").as_deref() == Ok("1") {
            eprintln!("[astra-sync][templates] merge_remote: replaced cache with {n} row(s)");
        }
        Ok(MergeResult {
            items_added: n,
            items_updated: 0,
            items_removed: 0,
            conflicts_auto_resolved: 0,
        })
    }

    fn resolve_conflict(
        &self,
        _local: &SyncPayload,
        _remote: &SyncPayload,
    ) -> Result<SyncPayload, SyncError> {
        Err(SyncError::permanent(
            SyncDomain::Templates,
            "templates pull-only — no merge conflict resolution",
        ))
    }

    fn validate(&self, payload: &SyncPayload) -> Result<(), SyncError> {
        let computed = sha256_checksum(&payload.data);
        if computed != payload.checksum {
            return Err(SyncError::permanent(
                SyncDomain::Templates,
                format!(
                    "checksum mismatch: expected {}, got {}",
                    payload.checksum, computed
                ),
            ));
        }
        let _: Vec<PlanTemplateSyncRow> = serde_json::from_slice(&payload.data).map_err(|e| {
            SyncError::permanent(SyncDomain::Templates, format!("invalid template pack: {e}"))
        })?;
        Ok(())
    }

    fn envelope(&self) -> SyncEnvelope {
        self.envelope
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|_| SyncEnvelope::new(SyncDomain::Templates))
    }

    fn set_envelope(&self, envelope: SyncEnvelope) {
        if let Ok(mut e) = self.envelope.lock() {
            *e = envelope;
        }
    }

    fn has_dirty_data(&self) -> bool {
        false
    }

    fn estimated_size(&self) -> usize {
        self.cache
            .lock()
            .map(|g| g.iter().map(|r| r.template_json.len() + 256).sum())
            .unwrap_or(0)
    }

    fn clear_dirty(&self) -> Result<(), SyncError> {
        Ok(())
    }
}

// ─── Preference Adapter ────────────────────────────────────────────────────────

/// Bidirectional preferences: local [`BTreeMap`] merged from cloud on pull, exported on push.
pub struct PreferenceAdapter {
    local: Arc<Mutex<BTreeMap<String, String>>>,
    envelope: Arc<Mutex<SyncEnvelope>>,
}

impl PreferenceAdapter {
    pub fn new(local: Arc<Mutex<BTreeMap<String, String>>>) -> Self {
        Self {
            local,
            envelope: Arc::new(Mutex::new(SyncEnvelope::new(SyncDomain::Preferences))),
        }
    }

    pub fn store(&self) -> Arc<Mutex<BTreeMap<String, String>>> {
        Arc::clone(&self.local)
    }
}

#[async_trait]
impl DomainAdapter for PreferenceAdapter {
    fn domain(&self) -> SyncDomain {
        SyncDomain::Preferences
    }

    fn export_full(&self) -> Result<SyncPayload, SyncError> {
        let map = self
            .local
            .lock()
            .map_err(|e| SyncError::permanent(SyncDomain::Preferences, format!("lock: {e}")))?;
        let data = serde_json::to_vec(&*map).map_err(|e| {
            SyncError::permanent(
                SyncDomain::Preferences,
                format!("serialize preferences: {e}"),
            )
        })?;
        let item_count = map.len() as u32;
        let checksum = sha256_checksum(&data);
        Ok(SyncPayload {
            data,
            format: PayloadFormat::Full,
            checksum,
            item_count,
            compressed: false,
        })
    }

    fn export_delta(&self) -> Result<Option<SyncPayload>, SyncError> {
        Ok(None)
    }

    fn merge_remote(&self, remote: &SyncPayload) -> Result<MergeResult, SyncError> {
        let remote_map: BTreeMap<String, String> =
            serde_json::from_slice(&remote.data).map_err(|e| {
                SyncError::permanent(
                    SyncDomain::Preferences,
                    format!("deserialize preferences: {e}"),
                )
            })?;
        let mut added = 0u32;
        let mut updated = 0u32;
        if let Ok(mut local) = self.local.lock() {
            for (k, v) in remote_map {
                match local.insert(k, v) {
                    None => added += 1,
                    Some(_) => updated += 1,
                }
            }
        }
        Ok(MergeResult {
            items_added: added,
            items_updated: updated,
            items_removed: 0,
            conflicts_auto_resolved: 0,
        })
    }

    fn resolve_conflict(
        &self,
        _local: &SyncPayload,
        remote: &SyncPayload,
    ) -> Result<SyncPayload, SyncError> {
        Ok(remote.clone())
    }

    fn validate(&self, payload: &SyncPayload) -> Result<(), SyncError> {
        let computed = sha256_checksum(&payload.data);
        if computed != payload.checksum {
            return Err(SyncError::permanent(
                SyncDomain::Preferences,
                format!(
                    "checksum mismatch: expected {}, got {}",
                    payload.checksum, computed
                ),
            ));
        }
        let _: BTreeMap<String, String> = serde_json::from_slice(&payload.data).map_err(|e| {
            SyncError::permanent(
                SyncDomain::Preferences,
                format!("invalid preferences json: {e}"),
            )
        })?;
        Ok(())
    }

    fn envelope(&self) -> SyncEnvelope {
        self.envelope
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|_| SyncEnvelope::new(SyncDomain::Preferences))
    }

    fn set_envelope(&self, envelope: SyncEnvelope) {
        if let Ok(mut e) = self.envelope.lock() {
            *e = envelope;
        }
    }

    fn has_dirty_data(&self) -> bool {
        self.envelope().sync_state.is_dirty()
    }

    fn estimated_size(&self) -> usize {
        self.local
            .lock()
            .map(|m| m.iter().map(|(k, v)| k.len() + v.len()).sum())
            .unwrap_or(0)
    }

    fn clear_dirty(&self) -> Result<(), SyncError> {
        Ok(())
    }
}

// ─── Task Adapter (Phase 3: lease-filtered mirror) ───────────────────────────

/// Local task mirror for [`SyncDomain::Tasks`]. Only tasks both **dirty** and **leased** in this
/// process ([`TaskLeaseHoldCache`]) are exported, so unleased edges do not push conflicting rows.
pub struct TaskAdapter {
    mirror: Arc<Mutex<BTreeMap<String, TaskRecord>>>,
    dirty: Arc<Mutex<HashSet<String>>>,
    edge_agent_id: Arc<str>,
    lease_holds: Arc<TaskLeaseHoldCache>,
    envelope: Arc<Mutex<SyncEnvelope>>,
}

impl TaskAdapter {
    pub fn new(
        mirror: Arc<Mutex<BTreeMap<String, TaskRecord>>>,
        dirty: Arc<Mutex<HashSet<String>>>,
        edge_agent_id: impl Into<Arc<str>>,
        lease_holds: Arc<TaskLeaseHoldCache>,
    ) -> Self {
        Self {
            mirror,
            dirty,
            edge_agent_id: edge_agent_id.into(),
            lease_holds,
            envelope: Arc::new(Mutex::new(SyncEnvelope::new(SyncDomain::Tasks))),
        }
    }

    pub fn mirror(&self) -> Arc<Mutex<BTreeMap<String, TaskRecord>>> {
        Arc::clone(&self.mirror)
    }

    /// Mark a task id dirty for sync (e.g. after local edits). Caller should ensure a lease exists.
    pub fn note_task_dirty(&self, task_id: impl AsRef<str>) {
        let id = task_id.as_ref().to_string();
        if let Ok(mut d) = self.dirty.lock() {
            d.insert(id);
        }
        if let Ok(mut env) = self.envelope.lock() {
            env.mark_dirty();
        }
    }
}

#[async_trait]
impl DomainAdapter for TaskAdapter {
    fn domain(&self) -> SyncDomain {
        SyncDomain::Tasks
    }

    fn export_full(&self) -> Result<SyncPayload, SyncError> {
        let held = self
            .lease_holds
            .held_task_ids_for_agent(&self.edge_agent_id);
        let mirror = self
            .mirror
            .lock()
            .map_err(|e| SyncError::permanent(SyncDomain::Tasks, format!("lock mirror: {e}")))?;
        let dirty = self
            .dirty
            .lock()
            .map_err(|e| SyncError::permanent(SyncDomain::Tasks, format!("lock dirty: {e}")))?;
        let tasks: Vec<TaskRecord> = dirty
            .iter()
            .filter(|id| held.contains(*id))
            .filter_map(|id| mirror.get(id).cloned())
            .collect();
        if tasks.is_empty() {
            return Err(SyncError::permanent(
                SyncDomain::Tasks,
                "no leased dirty tasks to export",
            ));
        }
        let body = TaskSyncPackBody {
            holder_agent_id: self.edge_agent_id.to_string(),
            tasks,
        };
        let data = serde_json::to_vec(&body).map_err(|e| {
            SyncError::permanent(SyncDomain::Tasks, format!("serialize tasks pack: {e}"))
        })?;
        let item_count = body.tasks.len() as u32;
        let checksum = sha256_checksum(&data);
        Ok(SyncPayload {
            data,
            format: PayloadFormat::Full,
            checksum,
            item_count,
            compressed: false,
        })
    }

    fn export_delta(&self) -> Result<Option<SyncPayload>, SyncError> {
        Ok(None)
    }

    fn merge_remote(&self, remote: &SyncPayload) -> Result<MergeResult, SyncError> {
        let tasks: Vec<TaskRecord> = serde_json::from_slice(&remote.data)
            .or_else(|_| serde_json::from_slice::<TaskSyncPackBody>(&remote.data).map(|b| b.tasks))
            .map_err(|e| {
                SyncError::permanent(
                    SyncDomain::Tasks,
                    format!("deserialize tasks pull pack: {e}"),
                )
            })?;
        let n = tasks.len() as u32;
        if let Ok(mut m) = self.mirror.lock() {
            for t in tasks {
                m.insert(t.task_id.clone(), t);
            }
        }
        Ok(MergeResult {
            items_added: n,
            items_updated: 0,
            items_removed: 0,
            conflicts_auto_resolved: 0,
        })
    }

    fn resolve_conflict(
        &self,
        _local: &SyncPayload,
        remote: &SyncPayload,
    ) -> Result<SyncPayload, SyncError> {
        Ok(remote.clone())
    }

    fn validate(&self, payload: &SyncPayload) -> Result<(), SyncError> {
        let computed = sha256_checksum(&payload.data);
        if computed != payload.checksum {
            return Err(SyncError::permanent(
                SyncDomain::Tasks,
                format!(
                    "checksum mismatch: expected {}, got {}",
                    payload.checksum, computed
                ),
            ));
        }
        let ok_array = serde_json::from_slice::<Vec<TaskRecord>>(&payload.data).is_ok();
        let ok_pack = serde_json::from_slice::<TaskSyncPackBody>(&payload.data).is_ok();
        if !ok_array && !ok_pack {
            return Err(SyncError::permanent(
                SyncDomain::Tasks,
                "invalid tasks payload (expected task array or holder pack)",
            ));
        }
        Ok(())
    }

    fn envelope(&self) -> SyncEnvelope {
        self.envelope
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|_| SyncEnvelope::new(SyncDomain::Tasks))
    }

    fn set_envelope(&self, envelope: SyncEnvelope) {
        if let Ok(mut e) = self.envelope.lock() {
            *e = envelope;
        }
    }

    fn has_dirty_data(&self) -> bool {
        let held = self
            .lease_holds
            .held_task_ids_for_agent(&self.edge_agent_id);
        let Ok(d) = self.dirty.lock() else {
            return false;
        };
        let Ok(m) = self.mirror.lock() else {
            return false;
        };
        d.iter().any(|id| held.contains(id) && m.contains_key(id))
    }

    fn estimated_size(&self) -> usize {
        self.mirror
            .lock()
            .map(|m| m.values().map(|t| t.title.len() + 256).sum())
            .unwrap_or(0)
    }

    fn clear_dirty(&self) -> Result<(), SyncError> {
        let held = self
            .lease_holds
            .held_task_ids_for_agent(&self.edge_agent_id);
        if let Ok(mut d) = self.dirty.lock() {
            d.retain(|id| !held.contains(id));
        }
        Ok(())
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::NoopTransport;

    fn make_test_learning_adapter() -> LearningAdapter {
        let entity_graph = Arc::new(Mutex::new(EntityGraph::default()));
        let pattern_library = Arc::new(Mutex::new(PatternLibrary::default()));
        let calibrator = Arc::new(Mutex::new(ProgressiveCalibrator::default()));
        let tool_health = Arc::new(Mutex::new(vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 10,
            total_failures: 1,
            failure_rate: 0.1,
            last_updated_epoch: 100,
        }]));
        LearningAdapter::new(entity_graph, pattern_library, calibrator, tool_health)
    }

    #[test]
    fn learning_adapter_export_full_roundtrip() {
        let adapter = make_test_learning_adapter();

        // Export full
        let payload = adapter.export_full().expect("export_full should succeed");
        assert_eq!(payload.format, PayloadFormat::Full);
        assert!(!payload.compressed);
        assert!(!payload.data.is_empty());
        assert!(!payload.checksum.is_empty());

        // Validate the payload we just exported
        adapter
            .validate(&payload)
            .expect("validate should succeed for own export");

        // Verify round-trip: deserialize the payload data
        let snapshot: LearningSnapshot =
            serde_json::from_slice(&payload.data).expect("should deserialize");
        assert_eq!(snapshot.version, 1);
        assert_eq!(snapshot.tool_health.len(), 1);
        assert_eq!(snapshot.tool_health[0].name, "bash");
    }

    #[test]
    fn learning_adapter_export_delta_when_clean() {
        let adapter = make_test_learning_adapter();

        // No dirty data → should return None
        let delta = adapter.export_delta().expect("export_delta should succeed");
        assert!(delta.is_none(), "clean adapter should export no delta");
    }

    #[test]
    fn learning_adapter_validate_detects_bad_checksum() {
        let adapter = make_test_learning_adapter();
        let mut payload = adapter.export_full().expect("export_full should succeed");

        // Corrupt the checksum
        payload.checksum = "bad_checksum".to_string();
        let err = adapter.validate(&payload);
        assert!(err.is_err(), "should fail with bad checksum");
        assert!(
            err.unwrap_err().message.contains("checksum mismatch"),
            "error should mention checksum"
        );
    }

    #[test]
    fn learning_adapter_validate_detects_bad_data() {
        let adapter = make_test_learning_adapter();
        let bad_data = b"not valid json";
        let checksum = sha256_checksum(bad_data);
        let payload = SyncPayload {
            data: bad_data.to_vec(),
            format: PayloadFormat::Full,
            checksum,
            item_count: 0,
            compressed: false,
        };
        let err = adapter.validate(&payload);
        assert!(err.is_err(), "should fail with invalid JSON");
        assert!(
            err.unwrap_err().message.contains("invalid payload"),
            "error should mention invalid payload"
        );
    }

    #[test]
    fn learning_adapter_merge_remote() {
        let adapter = make_test_learning_adapter();

        // Create a remote snapshot to merge
        let remote_snapshot = LearningSnapshot {
            version: 1,
            snapshot_epoch: 200,
            entities: vec![],
            patterns: vec![],
            calibration: None,
            tool_health: vec![ToolHealthEntry {
                name: "curl".to_string(),
                total_calls: 5,
                total_failures: 0,
                failure_rate: 0.0,
                last_updated_epoch: 200,
            }],
            active_canary: None,
        };
        let data = serde_json::to_vec(&remote_snapshot).unwrap();
        let checksum = sha256_checksum(&data);
        let payload = SyncPayload {
            data,
            format: PayloadFormat::Full,
            checksum,
            item_count: 1,
            compressed: false,
        };

        let result = adapter
            .merge_remote(&payload)
            .expect("merge_remote should succeed");

        // curl entry should have been added
        let health = adapter.tool_health.lock().unwrap();
        assert!(
            health.iter().any(|h| h.name == "curl"),
            "merged tool_health should contain 'curl'"
        );
        // Original bash entry should still be there
        assert!(
            health.iter().any(|h| h.name == "bash"),
            "original 'bash' entry should remain"
        );
        assert_eq!(health.len(), 2);
        // MergeResult should be reasonable
        assert_eq!(result.items_removed, 0);
    }

    #[test]
    fn learning_adapter_has_dirty_data_checks_modules() {
        let adapter = make_test_learning_adapter();
        // Fresh adapter should not be dirty
        assert!(
            !adapter.has_dirty_data(),
            "fresh adapter should not have dirty data"
        );
    }

    #[test]
    fn learning_adapter_envelope_roundtrip() {
        let adapter = make_test_learning_adapter();
        let mut env = adapter.envelope();
        assert_eq!(env.domain, SyncDomain::Learning);
        assert!(env.sync_state.is_clean());

        env.mark_dirty();
        adapter.set_envelope(env.clone());
        let retrieved = adapter.envelope();
        assert!(retrieved.sync_state.is_dirty());
    }

    #[test]
    fn noop_transport_integration() {
        // Verify NoopTransport satisfies CloudTransport
        let transport: Arc<dyn CloudTransport> = Arc::new(NoopTransport);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            assert!(transport.health_check().await, "noop should be healthy");

            let adapter = make_test_learning_adapter();
            let payload = adapter.export_full().unwrap();

            let push_result = transport
                .push("user1", SyncDomain::Learning, &payload, None)
                .await
                .expect("noop push should succeed");
            assert!(push_result.success);

            let pull_result = transport
                .pull("user1", SyncDomain::Learning)
                .await
                .expect("noop pull should succeed");
            assert!(pull_result.payload.is_none());
        });
    }

    #[test]
    fn event_adapter_is_always_clean() {
        let adapter = EventAdapter::new(event_ingestion::IngestionSender::disconnected());
        assert_eq!(adapter.domain(), SyncDomain::Events);
        assert!(!adapter.has_dirty_data());
        assert!(adapter.export_full().is_err());
        assert!(adapter.export_delta().unwrap().is_none());
        let result = adapter
            .merge_remote(&SyncPayload {
                data: vec![],
                format: PayloadFormat::Full,
                checksum: String::new(),
                item_count: 0,
                compressed: false,
            })
            .unwrap();
        assert_eq!(result.items_added, 0);
        assert_eq!(result.items_updated, 0);
        assert_eq!(result.items_removed, 0);
    }

    #[test]
    fn template_adapter_merge_replaces_cache() {
        use astra_services::state_sync::PlanTemplateSyncRow;

        let cache = Arc::new(Mutex::new(Vec::<PlanTemplateSyncRow>::new()));
        let adapter = TemplateAdapter::new(Arc::clone(&cache));
        let row = PlanTemplateSyncRow {
            template_id: "t1".into(),
            user_id: Some("u1".into()),
            goal_pattern: "fix bug".into(),
            project_type: Some("rust".into()),
            template_json: "{}".into(),
            success_rate: 0.9,
            avg_completion_time: None,
            use_count: 3,
        };
        let data = serde_json::to_vec(&vec![row.clone()]).unwrap();
        let checksum = sha256_checksum(&data);
        let payload = SyncPayload {
            data,
            format: PayloadFormat::Full,
            checksum,
            item_count: 1,
            compressed: false,
        };
        adapter.validate(&payload).unwrap();
        adapter.merge_remote(&payload).unwrap();
        let g = cache.lock().unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].template_id, "t1");
        assert!(adapter.export_full().is_err());
        assert!(!adapter.has_dirty_data());
    }

    #[test]
    fn preference_adapter_export_merge_roundtrip() {
        let store = Arc::new(Mutex::new(BTreeMap::from([("k1".into(), "local".into())])));
        let adapter = PreferenceAdapter::new(Arc::clone(&store));
        let mut env = adapter.envelope();
        env.mark_dirty();
        adapter.set_envelope(env);
        assert!(adapter.has_dirty_data());

        let payload = adapter.export_full().unwrap();
        adapter.validate(&payload).unwrap();

        let remote_store = Arc::new(Mutex::new(BTreeMap::from([("k2".into(), "cloud".into())])));
        let remote_adapter = PreferenceAdapter::new(remote_store);
        let remote_payload = remote_adapter.export_full().unwrap();

        adapter.merge_remote(&remote_payload).unwrap();
        let g = store.lock().unwrap();
        assert_eq!(g.get("k1"), Some(&"local".to_string()));
        assert_eq!(g.get("k2"), Some(&"cloud".to_string()));
    }

    #[test]
    fn task_adapter_export_requires_lease_hold() {
        use astra_services::TaskStatus;

        let mirror = Arc::new(Mutex::new(BTreeMap::new()));
        let dirty = Arc::new(Mutex::new(HashSet::new()));
        let holds = Arc::new(TaskLeaseHoldCache::default());
        let adapter = TaskAdapter::new(
            Arc::clone(&mirror),
            Arc::clone(&dirty),
            "agent-a",
            Arc::clone(&holds),
        );

        mirror.lock().unwrap().insert(
            "t1".into(),
            TaskRecord {
                task_id: "t1".into(),
                user_id: "u1".into(),
                session_id: None,
                parent_task_id: None,
                title: "x".into(),
                description: None,
                status: TaskStatus::Pending,
                progress_pct: 0,
                items_done: 0,
                items_total: 0,
                plan: None,
                checkpoint: None,
                error_message: None,
                created_at: "a".into(),
                updated_at: "b".into(),
                completed_at: None,
                user_rating: None,
                completion_time_sec: None,
                replan_count: 0,
                auto_adjustments: 0,
                outcome: None,
                project_type: None,
                goal_pattern: None,
                agent_id: None,
            },
        );
        adapter.note_task_dirty("t1");
        assert!(adapter.export_full().is_err());

        holds.record_hold("agent-a", "t1");
        let payload = adapter.export_full().expect("export with hold");
        assert!(!payload.data.is_empty());
    }
}
