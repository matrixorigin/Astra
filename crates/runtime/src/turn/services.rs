use crate::data_layer::storage::{
    AUXILIARY_EVENT_COLLISION_SOURCE, AgentEventCaptureAttempt, auxiliary_turn_event_payload_hash,
    classify_agent_event_capture_attempts, insert_trace_events,
};
use crate::*;
use astra_core::canonical_names::metadata_tool_name;
use astra_services::observation_capture::DurableCaptureOutcome;
use astra_turn_core::trace_event::{TraceEvent, TraceEventWriter, TraceWriteError};
use sqlx::Acquire;

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnObserverWorker;

#[derive(Clone, Debug)]
pub struct DatabaseTurnObserverWorker {
    pub(crate) base_url: String,
    pub(crate) master_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnHookDbWriter {
    pool: Option<SharedPool>,
    #[cfg(feature = "e2e-hooks")]
    retained_write_hook: Option<Arc<astra_services::decisions::RetainedWriteTestHook>>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnAuxiliaryEventWriter {
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTraceEventWriter {
    pool: Option<SharedPool>,
}

impl DatabaseTurnHookDbWriter {
    #[cfg(feature = "e2e-hooks")]
    pub fn with_retained_write_test_hook(
        mut self,
        hook: Arc<astra_services::decisions::RetainedWriteTestHook>,
    ) -> Self {
        self.retained_write_hook = Some(hook);
        self
    }

    pub fn new(_matrixone: MatrixOneSettings) -> Self {
        Self {
            pool: None,
            #[cfg(feature = "e2e-hooks")]
            retained_write_hook: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, String> {
        self.pool
            .as_ref()
            .map(|p| p.get().clone())
            .ok_or_else(|| "shared pool not configured".to_string())
    }
}

impl DatabaseTurnAuxiliaryEventWriter {
    pub fn new(_matrixone: MatrixOneSettings) -> Self {
        Self { pool: None }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, String> {
        self.pool
            .as_ref()
            .map(|p| p.get().clone())
            .ok_or_else(|| "shared pool not configured".to_string())
    }
}

impl DatabaseTraceEventWriter {
    pub fn new(_matrixone: MatrixOneSettings) -> Self {
        Self { pool: None }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, TraceWriteError> {
        self.pool
            .as_ref()
            .map(|p| p.get().clone())
            .ok_or_else(|| TraceWriteError::Unavailable("shared pool not configured".to_string()))
    }
}

type SessionEventDeltas = std::collections::BTreeMap<(String, String), (i64, Option<String>)>;

#[derive(Debug, Default)]
pub(crate) struct TraceEventPersistOutcome {
    pub(crate) session_event_deltas: SessionEventDeltas,
    pub(crate) event_outcomes: std::collections::BTreeMap<String, DurableCaptureOutcome>,
}

impl TraceEventPersistOutcome {
    pub(crate) fn accepts_projection(&self, event_id: &str) -> bool {
        matches!(
            self.event_outcomes.get(event_id),
            Some(DurableCaptureOutcome::Inserted | DurableCaptureOutcome::Replayed)
        )
    }

    pub(crate) fn merge(&mut self, other: Self) {
        for (key, (delta, last_event_id)) in other.session_event_deltas {
            let entry = self.session_event_deltas.entry(key).or_default();
            entry.0 += delta;
            if last_event_id.is_some() {
                entry.1 = last_event_id;
            }
        }
        for (event_id, incoming) in other.event_outcomes {
            self.event_outcomes
                .entry(event_id)
                .and_modify(|current| {
                    if !matches!(current, DurableCaptureOutcome::Collision { .. })
                        && (matches!(&incoming, DurableCaptureOutcome::Collision { .. })
                            || (*current == DurableCaptureOutcome::Replayed
                                && incoming == DurableCaptureOutcome::Inserted))
                    {
                        *current = incoming.clone();
                    }
                })
                .or_insert(incoming);
        }
    }
}

async fn admit_event_owners_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    owners: impl IntoIterator<Item = (String, String)>,
) -> Result<(), String> {
    let owners = owners
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    for (user_id, session_id) in owners {
        astra_services::storage::admit_session_event_write(tx, &session_id, &user_id, true)
            .await
            .map_err(|error| format!("admit session event write for {session_id}: {error}"))?;
    }
    Ok(())
}

async fn apply_touched_session_deltas_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    deltas: &SessionEventDeltas,
) -> Result<(), String> {
    for ((user_id, session_id), (delta, last_event_id)) in deltas {
        if *delta <= 0 {
            continue;
        }
        // Admission already created/locked this session in the same transaction.
        // Updating its count needs neither another fence read nor a root upsert.
        astra_services::storage::bump_agent_session_event_count(
            &mut **tx,
            session_id,
            user_id,
            *delta,
            last_event_id.as_deref(),
        )
        .await
        .map_err(|error| format!("apply agent session event delta for {session_id}: {error}"))?;
    }
    Ok(())
}

impl DatabaseTurnObserverWorker {
    pub fn new(base_url: String, master_key: Option<String>) -> Self {
        Self {
            base_url,
            master_key,
        }
    }
}

#[derive(serde::Serialize)]
struct MemoryExtractionObservePayload<'a> {
    messages: &'a [serde_json::Map<String, serde_json::Value>],
    session_id: &'a str,
}

fn encode_memory_extraction_observe_payload(
    request: &TurnObserverRequest,
) -> Result<Vec<u8>, String> {
    let site = astra_core::history_work::HistoryWorkSite::MemoryExtractionPayloadSerialization;
    let result = serde_json::to_vec(&MemoryExtractionObservePayload {
        messages: &request.messages,
        session_id: &request.session_id,
    });
    match result {
        Ok(payload) => {
            if astra_core::history_work::instrumentation_enabled() {
                astra_core::history_work::record_operation(
                    site,
                    payload.len().try_into().unwrap_or(u64::MAX),
                    request.messages.len().try_into().unwrap_or(u64::MAX),
                    0,
                );
            }
            Ok(payload)
        }
        Err(error) => {
            astra_core::history_work::record_serialization_failure(site, &error);
            Err(format!("serialize memoria observer payload: {error}"))
        }
    }
}

fn reserve_memory_extraction_payload(
    payload: &[u8],
) -> astra_core::history_work::QueueBytesReservation {
    astra_core::history_work::QueueBytesReservation::for_site(
        astra_core::history_work::HistoryWorkSite::MemoryExtractionQueue,
        payload.len().try_into().unwrap_or(u64::MAX),
    )
}

#[async_trait]
impl TraceEventWriter for DatabaseTraceEventWriter {
    async fn write(&self, event: TraceEvent) -> Result<(), TraceWriteError> {
        self.write_many(vec![event]).await
    }

    async fn write_many(&self, events: Vec<TraceEvent>) -> Result<(), TraceWriteError> {
        if events.is_empty() {
            return Ok(());
        }
        let pool = self.get_pool()?;
        let mut connection = astra_services::CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(|error| TraceWriteError::Persist(error.to_string()))?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|error| TraceWriteError::Persist(error.to_string()))?;
        let outcome = DatabaseTraceEventWriter::write_many_in_tx(&mut tx, events).await?;
        apply_touched_session_deltas_in_tx(&mut tx, &outcome.session_event_deltas)
            .await
            .map_err(TraceWriteError::Persist)?;
        tx.commit()
            .await
            .map_err(|error| TraceWriteError::Persist(error.to_string()))?;
        connection.release();
        Ok(())
    }
}

impl DatabaseTraceEventWriter {
    /// Variant of [`write_many`] that uses an existing transaction instead of
    /// creating its own. The caller owns commit/rollback.
    pub(crate) async fn write_many_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        events: Vec<TraceEvent>,
    ) -> Result<TraceEventPersistOutcome, TraceWriteError> {
        if events.is_empty() {
            return Ok(TraceEventPersistOutcome::default());
        }
        admit_event_owners_in_tx(
            tx,
            events
                .iter()
                .map(|event| (event.user_id.clone(), event.session_id.clone())),
        )
        .await
        .map_err(TraceWriteError::Persist)?;
        Self::write_many_in_tx_after_admission(tx, events, None).await
    }

    /// Persist trace rows after the caller has already admitted their exact
    /// owner in this same transaction. Canonical run settlement and terminal
    /// trace repair both hold that lock before reaching this writer; repeating
    /// session admission here only performs the same fence/status reads again.
    ///
    /// The owner pair is required by those callers so a future builder cannot
    /// accidentally bypass admission for a mixed-user batch.
    pub(crate) async fn write_many_in_admitted_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        admitted_user_id: &str,
        admitted_session_id: &str,
        events: Vec<TraceEvent>,
    ) -> Result<TraceEventPersistOutcome, TraceWriteError> {
        Self::write_many_in_tx_after_admission(
            tx,
            events,
            Some((admitted_user_id, admitted_session_id)),
        )
        .await
    }

    async fn write_many_in_tx_after_admission(
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        events: Vec<TraceEvent>,
        admitted_owner: Option<(&str, &str)>,
    ) -> Result<TraceEventPersistOutcome, TraceWriteError> {
        if events.is_empty() {
            return Ok(TraceEventPersistOutcome::default());
        }
        if let Some((admitted_user_id, admitted_session_id)) = admitted_owner
            && events.iter().any(|event| {
                event.user_id != admitted_user_id || event.session_id != admitted_session_id
            })
        {
            return Err(TraceWriteError::Persist(
                "admitted trace batch contains a different owner".to_string(),
            ));
        }
        let mut by_session = std::collections::BTreeMap::<(String, String), Vec<TraceEvent>>::new();
        for event in events {
            by_session
                .entry((event.user_id.clone(), event.session_id.clone()))
                .or_default()
                .push(event);
        }
        let mut persist_outcome = TraceEventPersistOutcome::default();
        for ((user_id, session_id), events) in by_session {
            let outcome = insert_trace_events(tx, &events)
                .await
                .map_err(|error| TraceWriteError::Persist(error.to_string()))?;
            let mut session_outcome = TraceEventPersistOutcome {
                event_outcomes: outcome.event_outcomes,
                ..Default::default()
            };
            if outcome.inserted > 0 {
                session_outcome.session_event_deltas.insert(
                    (user_id, session_id),
                    (
                        i64::try_from(outcome.inserted).unwrap_or(i64::MAX),
                        outcome.last_inserted_event_id,
                    ),
                );
            }
            // Event identity is owner-scoped, not session-scoped. A multi-session
            // write can therefore observe the same ID as an exact replay in its
            // stored session and as a collision in another session; collision
            // must dominate before any caller projects derived content.
            persist_outcome.merge(session_outcome);
        }
        // The transaction owner applies insertion deltas before committing.
        // Replayed events never increment the count or require a COUNT(*) scan.
        Ok(persist_outcome)
    }
}

#[async_trait]
impl TurnHookDbWriter for DatabaseTurnHookDbWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        let Some(mut skill_selection) = plan.skill_selection else {
            return Ok(());
        };
        let pool = self.get_pool()?;
        // Resolve catalog metadata before opening the write transaction. This
        // lookup uses the pool itself; doing it after `begin()` would hold one
        // connection while waiting for a second connection and can deadlock a
        // small pool (and unnecessarily consumes two leases in production).
        let skill_versions = resolve_active_skill_versions(
            &pool,
            skill_selection
                .selected_skills
                .iter()
                .map(String::as_str)
                .collect(),
        )
        .await
        .map_err(|error| error.to_string())?;
        if let Some(first_skill_name) = skill_selection.selected_skills.first()
            && let Some(skill_version) = skill_versions.get(first_skill_name)
        {
            skill_selection.skill_version = Some(skill_version.clone());
        }
        let mut connection = astra_services::CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(|error| error.to_string())?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|error| error.to_string())?;
        let result = async {
            astra_services::storage::admit_session_event_write(
                &mut tx,
                &skill_selection.session_id,
                &skill_selection.user_id,
                false,
            )
            .await?;
            insert_turn_skill_selection(&mut tx, &skill_selection).await?;
            #[cfg(feature = "e2e-hooks")]
            if let Some(hook) = &self.retained_write_hook
                && hook
                    .after_insert(&skill_selection.user_id, &skill_selection.session_id)
                    .await
            {
                return Err(sqlx::Error::Protocol(
                    "injected retained write failure".into(),
                ));
            }
            Ok::<(), sqlx::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                tx.commit().await.map_err(|error| error.to_string())?;
                connection.release();
                Ok(())
            }
            Err(error) => {
                if tx.rollback().await.is_ok() {
                    connection.release();
                }
                Err(error.to_string())
            }
        }
    }
}

#[async_trait]
impl TurnObserverWorker for NoopTurnObserverWorker {
    async fn run(&self, _request: TurnObserverRequest) -> Result<(), String> {
        Ok(())
    }
}

#[async_trait]
impl TurnObserverWorker for DatabaseTurnObserverWorker {
    async fn run(&self, request: TurnObserverRequest) -> Result<(), String> {
        let Some(master_key) = self.master_key.as_ref() else {
            return Ok(());
        };
        if request.messages.is_empty() {
            return Ok(());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        // Serialize exactly once, then retain and send that same byte buffer.
        // Calling `.json(...)` here as well would hide the queue's actual byte
        // weight behind a second serializer-owned allocation.
        let payload = encode_memory_extraction_observe_payload(&request)?;
        let queue_reservation = reserve_memory_extraction_payload(&payload);
        let response = client
            .post(format!(
                "{}/v1/observe",
                self.base_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {master_key}"))
            .header("X-Impersonate-User", request.user_id)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await;
        drop(queue_reservation);
        let response = response.map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "memoria observer run failed: status={}",
                response.status()
            ))
        }
    }
}

#[async_trait]
impl TurnAuxiliaryEventWriter for DatabaseTurnAuxiliaryEventWriter {
    async fn persist_events(&self, events: Vec<TurnAuxiliaryEventRecord>) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        let pool = self.get_pool()?;
        let mut connection = astra_services::CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(|error| error.to_string())?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|error| error.to_string())?;
        admit_event_owners_in_tx(
            &mut tx,
            events
                .iter()
                .map(|event| (event.user_id.clone(), event.session_id.clone())),
        )
        .await?;
        let mut deltas = SessionEventDeltas::new();
        for event in events {
            let meta_tool_name = metadata_tool_name(event.metadata.as_ref());
            let meta_duration_ms = event
                .metadata
                .as_ref()
                .and_then(|v| v.get("duration_ms"))
                .and_then(|v| v.as_i64())
                .map(|v| v as i32);
            let payload_hash = auxiliary_turn_event_payload_hash(
                &event,
                meta_tool_name.as_deref(),
                meta_duration_ms,
            );
            let ingestion_write_id = Uuid::new_v4().to_string();
            let metadata_json = event.metadata.as_ref().map(|metadata| metadata.to_string());
            let result = query(
                "INSERT IGNORE INTO agent_events \
                 (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
                  parent_event_id, causal_chain_id, `metadata`, reasoning_content, \
                  meta_tool_name, meta_duration_ms, payload_hash, ingestion_write_id, created_at) \
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
            )
            .bind(&event.event_id)
            .bind(&event.session_id)
            .bind(&event.user_id)
            .bind(event.agent_id.as_deref().unwrap_or("astra-cli"))
            .bind(env!("CARGO_PKG_VERSION"))
            .bind(&event.event_type)
            .bind(&event.content)
            .bind(&event.parent_event_id)
            .bind(&event.causal_chain_id)
            .bind(&metadata_json)
            .bind(&event.reasoning_content)
            .bind(&meta_tool_name)
            .bind(meta_duration_ms)
            .bind(&payload_hash)
            .bind(&ingestion_write_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
            let _reported_rows_affected = result.rows_affected();
            let outcome = classify_agent_event_capture_attempts(
                &mut tx,
                &[AgentEventCaptureAttempt {
                    user_id: &event.user_id,
                    session_id: &event.session_id,
                    event_id: &event.event_id,
                    payload_hash: &payload_hash,
                }],
                &ingestion_write_id,
                AUXILIARY_EVENT_COLLISION_SOURCE,
            )
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| "missing auxiliary event capture outcome".to_string())?;
            if outcome == DurableCaptureOutcome::Inserted {
                crate::data_layer::storage::insert_agent_event_edges(
                    &mut *tx,
                    &event.user_id,
                    &event.session_id,
                    &event.event_id,
                    event.parent_event_id.as_deref(),
                    &event.parent_event_ids,
                )
                .await
                .map_err(|error| error.to_string())?;
                let entry = deltas
                    .entry((event.user_id.clone(), event.session_id.clone()))
                    .or_default();
                entry.0 += 1;
                entry.1 = Some(event.event_id.clone());
            }
        }
        apply_touched_session_deltas_in_tx(&mut tx, &deltas).await?;
        tx.commit().await.map_err(|error| error.to_string())?;
        connection.release();
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnHookDbWriter;

#[async_trait]
impl TurnHookDbWriter for NoopTurnHookDbWriter {
    async fn persist(&self, _plan: TurnHookDbPersistPlan) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnAuxiliaryEventWriter;

#[async_trait]
impl TurnAuxiliaryEventWriter for NoopTurnAuxiliaryEventWriter {
    async fn persist_events(&self, _events: Vec<TurnAuxiliaryEventRecord>) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
static LIVE_TEST_SETTINGS: tokio::sync::OnceCell<MatrixOneSettings> =
    tokio::sync::OnceCell::const_new();

#[cfg(test)]
pub(crate) async fn setup_live_pool_for_test() -> SharedPool {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    let settings = LIVE_TEST_SETTINGS
        .get_or_init(|| async {
            let settings = MatrixOneSettings::from_env();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".to_string());
            astra_services::ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema");
            settings
        })
        .await;
    // SQLx pools own runtime-bound maintenance tasks. Each #[tokio::test]
    // therefore receives a fresh pool even though schema bootstrap is shared.
    SharedPool::new(settings).await.expect("SharedPool::new")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::Row;
    use uuid::Uuid;

    #[test]
    fn metadata_tool_name_none() {
        assert!(metadata_tool_name(None).is_none());
    }

    #[test]
    #[serial_test::serial(history_work)]
    fn memory_extraction_queue_uses_the_single_http_body_and_releases_it() {
        let request = TurnObserverRequest {
            user_id: "user-1".to_string(),
            session_id: "session-1".to_string(),
            messages: vec![
                json!({"role": "user", "content": "hello"})
                    .as_object()
                    .expect("message object")
                    .clone(),
            ],
            turn_count: 1,
            session_start: None,
        };
        let payload =
            encode_memory_extraction_observe_payload(&request).expect("serialize observer body");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&payload).expect("valid JSON body"),
            json!({
                "messages": [{"role": "user", "content": "hello"}],
                "session_id": "session-1",
            })
        );
        let expected_bytes = payload.len().try_into().unwrap_or(u64::MAX);
        let scenario =
            astra_core::history_work::HistoryWorkScenario::begin("memory-extraction-queue-drop")
                .expect("exclusive history-work scenario");

        {
            let reservation = reserve_memory_extraction_payload(&payload);
            assert_eq!(reservation.bytes(), expected_bytes);
        }

        let report = scenario.finish().expect("history-work report");
        let measurement = report
            .scoped
            .measurement(astra_core::history_work::HistoryWorkSite::MemoryExtractionQueue);
        assert_eq!(measurement.events, 1);
        assert_eq!(measurement.bytes, expected_bytes);
        assert_eq!(measurement.queue_peak_bytes, expected_bytes);
        assert_eq!(measurement.queue_current_bytes, 0);
    }

    #[test]
    fn metadata_tool_name_from_tool_name() {
        let v = json!({"tool_name": " bash "});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "bash");
    }

    #[test]
    fn metadata_tool_name_does_not_use_name_alias() {
        let v = json!({"name": "read_file"});
        assert!(metadata_tool_name(Some(&v)).is_none());
    }

    #[test]
    fn metadata_tool_name_ignores_ambiguous_name_when_tool_name_exists() {
        let v = json!({"tool_name": "preferred", "name": "read_file"});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "preferred");
    }

    #[test]
    fn metadata_tool_name_trims_quotes() {
        let v = json!({"tool_name": "\"bash\""});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "bash");
    }

    #[test]
    fn metadata_tool_name_empty_after_trim() {
        let v = json!({"tool_name": "\"\""});
        assert!(metadata_tool_name(Some(&v)).is_none());
    }

    #[test]
    fn metadata_tool_name_missing_both_fields() {
        let v = json!({"other": "field"});
        assert!(metadata_tool_name(Some(&v)).is_none());
    }

    #[test]
    fn metadata_tool_name_non_string_value() {
        let v = json!({"tool_name": 42});
        assert!(metadata_tool_name(Some(&v)).is_none());
    }

    fn trace_event(event_id: &str, user_id: &str, session_id: &str) -> TraceEvent {
        TraceEvent::new(event_id, session_id, user_id, "trace", "runtime")
    }

    fn auxiliary_event(
        event_id: &str,
        user_id: &str,
        session_id: &str,
        causal_chain_id: &str,
        event_type: &str,
        content: &str,
        parent_event_id: Option<&str>,
    ) -> TurnAuxiliaryEventRecord {
        TurnAuxiliaryEventRecord {
            event_id: event_id.to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            agent_id: None,
            event_type: event_type.to_string(),
            content: content.to_string(),
            parent_event_id: parent_event_id.map(str::to_string),
            parent_event_ids: parent_event_id
                .map(|id| vec![id.to_string()])
                .unwrap_or_default(),
            causal_chain_id: causal_chain_id.to_string(),
            metadata: None,
            reasoning_content: None,
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn auxiliary_events_increment_event_count_by_insert_delta_on_live_matrixone() {
        let shared = setup_live_pool_for_test().await;
        let pool = shared.get().clone();
        let settings = MatrixOneSettings::from_env();
        let suffix = Uuid::new_v4().to_string();
        let session_id = format!("turn-writer-{suffix}");
        let user_id = format!("user-{suffix}");
        let causal_chain_id = format!("chain-{suffix}");
        let aux_duplicate_event_id = format!("aux-dup-{suffix}");
        let aux_unique_event_id = format!("aux-unique-{suffix}");

        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
             VALUES (?, ?, 'turn-writer-delta-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert session");

        let aux_writer =
            DatabaseTurnAuxiliaryEventWriter::new(settings.clone()).with_pool(shared.clone());
        aux_writer
            .persist_events(vec![
                auxiliary_event(
                    &aux_duplicate_event_id,
                    &user_id,
                    &session_id,
                    &causal_chain_id,
                    "system_note",
                    "first duplicate",
                    None,
                ),
                auxiliary_event(
                    &aux_duplicate_event_id,
                    &user_id,
                    &session_id,
                    &causal_chain_id,
                    "system_note",
                    "second duplicate",
                    None,
                ),
                auxiliary_event(
                    &aux_unique_event_id,
                    &user_id,
                    &session_id,
                    &causal_chain_id,
                    "system_note",
                    "unique",
                    Some(&aux_duplicate_event_id),
                ),
            ])
            .await
            .expect("persist auxiliary events");
        aux_writer
            .persist_events(vec![auxiliary_event(
                &aux_duplicate_event_id,
                &user_id,
                &session_id,
                &causal_chain_id,
                "system_note",
                "first duplicate",
                None,
            )])
            .await
            .expect("exact replay must not increment the count or replace the tail");

        let row = sqlx::query(
            "SELECT event_count, last_event_id FROM agent_sessions WHERE session_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("load session count");
        assert_eq!(
            row.try_get::<i64, _>("event_count")
                .expect("decode event_count"),
            2,
            "writers must add only actual inserted rows; duplicate INSERT IGNORE rows must not bump"
        );
        assert_eq!(
            row.try_get::<String, _>("last_event_id")
                .expect("decode last_event_id"),
            aux_unique_event_id
        );

        let actual_events = sqlx::query(
            "SELECT COUNT(*) AS c FROM agent_events WHERE session_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("count persisted events")
        .try_get::<i64, _>("c")
        .expect("decode event count");
        assert_eq!(actual_events, 2);

        let collision_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_identity_collisions \
             WHERE user_id = ? AND identity_kind = 'agent_event'",
        )
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("count hash-fenced writer collisions");
        assert_eq!(
            collision_count, 1,
            "changed auxiliary stable IDs must be classified as collisions"
        );

        sqlx::query("DELETE FROM observation_identity_collisions WHERE user_id = ?")
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup event writer collision receipts");
        sqlx::query("DELETE FROM agent_event_edges WHERE session_id = ? AND user_id = ?")
            .bind(&session_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup auxiliary fixture event edges");
        sqlx::query("DELETE FROM agent_events WHERE session_id = ? AND user_id = ?")
            .bind(&session_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup event count fixture agent_events");
        sqlx::query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&session_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup event count fixture agent_sessions");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn trace_batch_tail_tracks_the_last_new_event_in_both_replay_orders() {
        let shared = setup_live_pool_for_test().await;
        let pool = shared.get().clone();
        let settings = MatrixOneSettings::from_env();
        let suffix = Uuid::new_v4().to_string();
        let user_id = format!("trace-tail-user-{suffix}");
        let existing_first_session = format!("trace-tail-existing-first-{suffix}");
        let existing_last_session = format!("trace-tail-existing-last-{suffix}");
        let existing_first = format!("trace-existing-first-{suffix}");
        let new_after = format!("trace-new-after-{suffix}");
        let new_before = format!("trace-new-before-{suffix}");
        let existing_last = format!("trace-existing-last-{suffix}");

        for session_id in [&existing_first_session, &existing_last_session] {
            sqlx::query(
                "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
                 VALUES (?, ?, 'trace-tail-it', 'active', 0)",
            )
            .bind(session_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("insert trace-tail session");
        }

        let writer = DatabaseTraceEventWriter::new(settings).with_pool(shared);
        let existing_first_event = trace_event(&existing_first, &user_id, &existing_first_session);
        writer
            .write(existing_first_event.clone())
            .await
            .expect("persist existing-first fixture");
        writer
            .write_many(vec![
                existing_first_event,
                trace_event(&new_after, &user_id, &existing_first_session),
            ])
            .await
            .expect("persist existing-then-new batch");
        let existing_last_event = trace_event(&existing_last, &user_id, &existing_last_session);
        writer
            .write(existing_last_event.clone())
            .await
            .expect("persist existing-last fixture");
        writer
            .write_many(vec![
                trace_event(&new_before, &user_id, &existing_last_session),
                existing_last_event,
            ])
            .await
            .expect("persist new-then-existing batch");

        for (session_id, expected_tail) in [
            (&existing_first_session, &new_after),
            (&existing_last_session, &new_before),
        ] {
            let row = sqlx::query(
                "SELECT event_count, last_event_id FROM agent_sessions \
                 WHERE user_id = ? AND session_id = ?",
            )
            .bind(&user_id)
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("load trace-tail session summary");
            assert_eq!(row.try_get::<i64, _>("event_count").unwrap(), 2);
            assert_eq!(
                row.try_get::<String, _>("last_event_id").unwrap(),
                *expected_tail
            );
        }

        sqlx::query("DELETE FROM agent_event_edges WHERE user_id = ?")
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup trace-tail edges");
        sqlx::query("DELETE FROM agent_events WHERE user_id = ?")
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup trace-tail events");
        sqlx::query("DELETE FROM agent_sessions WHERE user_id = ?")
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup trace-tail sessions");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn admitted_trace_batch_rejects_mixed_owner_without_writes() {
        let shared = setup_live_pool_for_test().await;
        let pool = shared.get().clone();
        let suffix = Uuid::new_v4().to_string();
        let first_user = format!("admitted-trace-user-a-{suffix}");
        let first_session = format!("admitted-trace-session-a-{suffix}");
        let second_user = format!("admitted-trace-user-b-{suffix}");
        let second_session = format!("admitted-trace-session-b-{suffix}");

        for (user_id, session_id) in [
            (&first_user, &first_session),
            (&second_user, &second_session),
        ] {
            sqlx::query(
                "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
                 VALUES (?, ?, 'admitted-trace-owner-it', 'active', 0)",
            )
            .bind(session_id)
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("insert admitted-trace owner session");
        }

        let mut connection = astra_services::CancellationSafePoolConnection::acquire(&pool)
            .await
            .expect("acquire admitted-trace transaction connection");
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .expect("begin admitted-trace transaction");
        admit_event_owners_in_tx(
            &mut tx,
            std::iter::once((first_user.clone(), first_session.clone())),
        )
        .await
        .expect("admit the exact trace owner");

        let error = DatabaseTraceEventWriter::write_many_in_admitted_tx(
            &mut tx,
            &first_user,
            &first_session,
            vec![
                trace_event("admitted-trace-first", &first_user, &first_session),
                trace_event("admitted-trace-mixed", &second_user, &second_session),
            ],
        )
        .await
        .expect_err("mixed-owner admitted batch must be rejected before insert");
        assert!(
            error.to_string().contains("different owner"),
            "unexpected mixed-owner error: {error}"
        );
        for (user_id, session_id) in [
            (&first_user, &first_session),
            (&second_user, &second_session),
        ] {
            let event_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_one(&mut *tx)
            .await
            .expect("count mixed-owner trace events before rollback");
            assert_eq!(event_count, 0, "mixed-owner batch must not insert events");

            let session_event_count: i64 = sqlx::query_scalar(
                "SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_one(&mut *tx)
            .await
            .expect("read mixed-owner session event count before rollback");
            assert_eq!(
                session_event_count, 0,
                "mixed-owner batch must not change session counters"
            );
        }
        tx.rollback()
            .await
            .expect("rollback mixed-owner admitted batch");
        connection.release();

        sqlx::query(
            "DELETE FROM agent_sessions WHERE (user_id = ? AND session_id = ?) \
             OR (user_id = ? AND session_id = ?)",
        )
        .bind(&first_user)
        .bind(&first_session)
        .bind(&second_user)
        .bind(&second_session)
        .execute(&pool)
        .await
        .expect("cleanup admitted-trace owner sessions");
    }

    /// Verify that the retained writers fail instantly when no pool is
    /// configured, rather than blocking on a 2s connect_matrixone() timeout.
    #[tokio::test]
    async fn no_pool_writers_fail_fast_without_timeout() {
        use std::time::Instant;

        let settings = MatrixOneSettings {
            host: "127.0.0.1".into(),
            port: 0,
            user: "x".into(),
            password: "x".into(),
            database: "x".into(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };

        let start = Instant::now();

        // An empty hook must succeed without even a configured pool.
        let w = DatabaseTurnHookDbWriter::new(settings.clone());
        let r = w.persist(TurnHookDbPersistPlan::default()).await;
        assert!(r.is_ok());

        // AuxiliaryEventWriter
        let w = DatabaseTurnAuxiliaryEventWriter::new(settings.clone());
        let r = w
            .persist_events(vec![TurnAuxiliaryEventRecord {
                event_id: "e4".into(),
                user_id: "u".into(),
                session_id: "s".into(),
                agent_id: None,
                event_type: "aux".into(),
                content: "x".into(),
                parent_event_id: None,
                parent_event_ids: vec![],
                causal_chain_id: "c".into(),
                metadata: None,
                reasoning_content: None,
            }])
            .await;
        assert!(r.is_err());

        // Remaining writers must complete in <100ms (previously each took 2s)
        assert!(
            start.elapsed().as_millis() < 100,
            "no-pool writers took {}ms — should be instant",
            start.elapsed().as_millis()
        );
    }
}
