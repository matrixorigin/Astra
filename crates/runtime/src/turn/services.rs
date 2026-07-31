use crate::data_layer::storage::{
    bump_agent_session_event_count, insert_trace_event, touch_agent_session_activity,
};
use crate::server::run::lifecycle::{
    TranscriptPersistItem, TranscriptPersistPayload, persist_session_transcript_items_inner_in_tx,
};
use crate::*;
use astra_core::canonical_names::metadata_tool_name;
use astra_turn_core::trace_event::{TraceEvent, TraceEventWriter, TraceWriteError};

fn validate_tool_lifecycle_event_type(event_type: &str) -> Result<(), String> {
    if matches!(
        event_type,
        "tool_call_started"
            | "tool_call_completed"
            | "tool_call_failed"
            | "tool_call_rejected"
            | "tool_call_reused"
            | "tool_call_suppressed"
            | "tool_call_deferred"
    ) {
        Ok(())
    } else {
        Err(format!(
            "non-canonical tool lifecycle event type: {event_type:?}"
        ))
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnSessionActivityWriter {
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnToolEventWriter {
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnHookDbWriter {
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnAuxiliaryEventWriter {
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnCoreEventWriter {
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTraceEventWriter {
    pool: Option<SharedPool>,
}

impl DatabaseTurnSessionActivityWriter {
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

impl DatabaseTurnToolEventWriter {
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

impl DatabaseTurnHookDbWriter {
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

impl DatabaseTurnCoreEventWriter {
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

fn record_session_event_delta(
    deltas: &mut std::collections::BTreeMap<(String, String), (i64, Option<String>)>,
    event: &TurnCoreEventRecord,
    last_event_id: Option<&str>,
) {
    let entry = deltas
        .entry((event.user_id.clone(), event.session_id.clone()))
        .or_default();
    entry.0 += 1;
    if let Some(last_event_id) = last_event_id {
        entry.1 = Some(last_event_id.to_string());
    }
}

fn bridge_transcript_item(event: &TurnCoreEventRecord) -> Option<TranscriptPersistItem> {
    let role = match event.event_type.as_str() {
        "user_query" => "user",
        "llm_response" => "assistant",
        // Runtime reconciliation is durable evidence, not a user utterance.
        // Its visible assistant result is still materialized by the paired
        // `llm_response` event below.
        _ => return None,
    };
    // Tool-only model rounds intentionally have no user-visible assistant
    // text. Persist their typed tool events, but do not materialize a blank
    // transcript row on every bridge continuation.
    if role == "assistant"
        && event.content.trim().is_empty()
        && event
            .reasoning_content
            .as_deref()
            .is_none_or(|reasoning| reasoning.trim().is_empty())
    {
        return None;
    }
    let payload = event
        .reasoning_content
        .as_ref()
        .filter(|reasoning| !reasoning.trim().is_empty())
        .map(|reasoning| TranscriptPersistPayload {
            reasoning: Some(reasoning.clone()),
            reasoning_status: Some("completed".to_string()),
            ..Default::default()
        });
    Some(TranscriptPersistItem {
        // CLI bridge runs are local runtime identities, not durable
        // `agent_runs` rows. A NULL run_id keeps them visible in both the
        // session transcript and its root-conversation projection.
        run_id: None,
        role,
        content: event.content.clone(),
        payload,
        source_event_id: event.event_id.clone(),
    })
}

impl DatabaseTurnReflectionLessonWriter {
    pub fn new(base_url: String, master_key: Option<String>) -> Self {
        Self {
            base_url,
            master_key,
        }
    }
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
impl TurnCoreEventWriter for DatabaseTurnCoreEventWriter {
    async fn persist(&self, plan: TurnCorePersistPlan) -> Result<TurnCorePersistOutcome, String> {
        if plan.user_query_event.is_none()
            && plan.llm_response_event.is_none()
            && plan.snapshot_link_plan.is_none()
        {
            return Ok(TurnCorePersistOutcome::default());
        }
        let pool = self.get_pool()?;
        let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
        let transcript_owner = plan
            .user_query_event
            .as_ref()
            .or(plan.llm_response_event.as_ref())
            .map(|event| (event.user_id.clone(), event.session_id.clone()));
        if let (Some(user), Some(assistant)) = (
            plan.user_query_event.as_ref(),
            plan.llm_response_event.as_ref(),
        ) && (user.user_id != assistant.user_id || user.session_id != assistant.session_id)
        {
            return Err("core turn events must share one transcript owner/session".to_string());
        }
        let transcript_items = plan
            .user_query_event
            .iter()
            .chain(plan.llm_response_event.iter())
            .filter_map(bridge_transcript_item)
            .collect::<Vec<_>>();
        let mut deltas =
            std::collections::BTreeMap::<(String, String), (i64, Option<String>)>::new();
        if let Some(event) = plan.user_query_event.as_ref() {
            if insert_core_turn_event(&mut tx, event)
                .await
                .map_err(|error| error.to_string())?
            {
                record_session_event_delta(&mut deltas, event, Some(&event.event_id));
            }
        }
        if let Some(event) = plan.llm_response_event.as_ref() {
            if insert_core_turn_event(&mut tx, event)
                .await
                .map_err(|error| error.to_string())?
            {
                record_session_event_delta(&mut deltas, event, Some(&event.event_id));
            }
        }
        if let Some((user_id, session_id)) = transcript_owner
            && !transcript_items.is_empty()
        {
            persist_session_transcript_items_inner_in_tx(
                &mut tx,
                &user_id,
                &session_id,
                &transcript_items,
            )
            .await
            .map_err(|error| format!("persist bridge transcript items: {error}"))?;
        }
        for ((user_id, session_id), (delta, last_event_id)) in deltas {
            bump_agent_session_event_count(
                &mut *tx,
                &session_id,
                &user_id,
                delta,
                last_event_id.as_deref(),
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        tx.commit().await.map_err(|error| error.to_string())?;
        if let Some(snapshot_link_plan) = plan.snapshot_link_plan.as_ref()
            && let Err(error) = update_snapshot_llm_ids(&pool, snapshot_link_plan).await
        {
            astra_core::agent_error!("bridge", "snapshot link update failed: {error}");
        }
        let outcome = TurnCorePersistOutcome {
            llm_response_event_id: plan.llm_response_event.map(|event| event.event_id),
        };
        Ok(outcome)
    }
}

#[async_trait]
impl TurnToolEventWriter for DatabaseTurnToolEventWriter {
    async fn persist(&self, plan: TurnToolEventPersistPlan) -> Result<(), String> {
        if plan.events.is_empty() {
            return Ok(());
        }
        for event in &plan.events {
            validate_tool_lifecycle_event_type(&event.event_type)?;
        }
        let pool = self.get_pool()?;
        let skill_versions = resolve_active_skill_versions(
            &pool,
            plan.events
                .iter()
                .filter_map(|event| event.skill_name.as_deref())
                .collect(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
        let mut deltas = std::collections::BTreeMap::<(String, String), i64>::new();
        for event in &plan.events {
            if insert_tool_turn_event(
                &mut tx,
                event,
                skill_versions.get(event.skill_name.as_deref().unwrap_or("")),
            )
            .await
            .map_err(|error| error.to_string())?
            {
                *deltas
                    .entry((event.user_id.clone(), event.session_id.clone()))
                    .or_default() += 1;
            }
        }
        for ((user_id, session_id), delta) in deltas {
            bump_agent_session_event_count(&mut *tx, &session_id, &user_id, delta, None)
                .await
                .map_err(|error| error.to_string())?;
        }
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(())
    }
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
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| TraceWriteError::Persist(error.to_string()))?;
        DatabaseTraceEventWriter::write_many_in_tx(&mut tx, events).await?;
        tx.commit()
            .await
            .map_err(|error| TraceWriteError::Persist(error.to_string()))?;
        Ok(())
    }
}

impl DatabaseTraceEventWriter {
    /// Variant of [`write_many`] that uses an existing transaction instead of
    /// creating its own. The caller owns commit/rollback.
    pub(crate) async fn write_many_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        events: Vec<TraceEvent>,
    ) -> Result<(), TraceWriteError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut touched_sessions =
            std::collections::BTreeMap::<(String, String), (i64, Option<String>)>::new();
        for event in &events {
            if insert_trace_event(tx, event)
                .await
                .map_err(|error| TraceWriteError::Persist(error.to_string()))?
            {
                let entry = touched_sessions
                    .entry((event.user_id.clone(), event.session_id.clone()))
                    .or_default();
                entry.0 += 1;
                entry.1 = Some(event.event_id.clone());
            }
        }
        for ((user_id, session_id), (delta, last_event_id)) in touched_sessions {
            bump_agent_session_event_count(
                &mut **tx,
                &session_id,
                &user_id,
                delta,
                last_event_id.as_deref(),
            )
            .await
            .map_err(|error| TraceWriteError::Persist(error.to_string()))?;
        }
        Ok(())
    }
}

#[async_trait]
impl TurnHookDbWriter for DatabaseTurnHookDbWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        if plan.decision_audit.is_none() && plan.skill_selection.is_none() {
            return Ok(());
        }
        let pool = self.get_pool()?;
        let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
        if let Some(decision_audit) = plan.decision_audit.as_ref() {
            insert_turn_decision_audit(&mut tx, decision_audit)
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Some(skill_selection) = plan.skill_selection.as_ref() {
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
            insert_turn_skill_selection(&mut tx, skill_selection)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(first_skill_name) = skill_selection.selected_skills.first()
                && let Some(skill_version) = skill_versions.get(first_skill_name)
            {
                update_turn_skill_selection_version(
                    &mut tx,
                    &skill_selection.event_id,
                    &skill_selection.user_id,
                    &skill_selection.session_id,
                    skill_version,
                )
                .await
                .map_err(|error| error.to_string())?;
            }
        }
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[async_trait]
impl TurnReflectionStateStore for InMemoryTurnReflectionStateStore {
    async fn mark_reflecting(&self, mark: TurnReflectionMark) -> Result<(), String> {
        self.state
            .lock()
            .await
            .insert(mark.session_id.clone(), mark);
        Ok(())
    }

    async fn pop_reflecting(&self, session_id: &str) -> Result<Option<TurnReflectionMark>, String> {
        Ok(self.state.lock().await.remove(session_id))
    }
}

#[async_trait]
impl TurnReflectionLessonWriter for NoopTurnReflectionLessonWriter {
    async fn persist_lesson(&self, _lesson: TurnReflectionLessonRecord) -> Result<(), String> {
        Ok(())
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
impl TurnReflectionLessonWriter for DatabaseTurnReflectionLessonWriter {
    async fn persist_lesson(&self, lesson: TurnReflectionLessonRecord) -> Result<(), String> {
        let Some(master_key) = self.master_key.as_ref() else {
            return Ok(());
        };
        let payload = serde_json::json!({
            "content": lesson.content,
            "memory_type": "procedural",
            "trust_tier": "T3",
            "session_id": lesson.session_id,
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?
            .post(format!(
                "{}/v1/memories",
                self.base_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {master_key}"))
            .header("X-Impersonate-User", lesson.user_id)
            .json(&payload)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "memoria lesson persist failed: status={}",
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
        let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
        let mut deltas = std::collections::BTreeMap::<(String, String), i64>::new();
        for event in events {
            let meta_tool_name = metadata_tool_name(event.metadata.as_ref());
            let meta_duration_ms = event
                .metadata
                .as_ref()
                .and_then(|v| v.get("duration_ms"))
                .and_then(|v| v.as_i64())
                .map(|v| v as i32);
            let metadata_json = event.metadata.as_ref().map(|metadata| metadata.to_string());
            let result = query(
                "INSERT IGNORE INTO agent_events \
                 (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
                  parent_event_id, causal_chain_id, `metadata`, reasoning_content, \
                  meta_tool_name, meta_duration_ms, created_at) \
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
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
            .bind(metadata_json)
            .bind(&event.reasoning_content)
            .bind(meta_tool_name)
            .bind(meta_duration_ms)
            .execute(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
            if result.rows_affected() > 0 {
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
                *deltas
                    .entry((event.user_id.clone(), event.session_id.clone()))
                    .or_default() += 1;
            }
        }
        for ((user_id, session_id), delta) in deltas {
            bump_agent_session_event_count(&mut *tx, &session_id, &user_id, delta, None)
                .await
                .map_err(|error| error.to_string())?;
        }
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[async_trait]
impl TurnSessionActivityWriter for DatabaseTurnSessionActivityWriter {
    async fn update_session_activity(
        &self,
        session_id: &str,
        user_id: &str,
        plan: SessionActivityUpdatePlan,
    ) -> Result<(), String> {
        let pool = self.get_pool()?;
        touch_agent_session_activity(&pool, session_id, user_id, plan.last_event_id.as_deref())
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnSessionActivityWriter;

#[async_trait]
impl TurnSessionActivityWriter for NoopTurnSessionActivityWriter {
    async fn update_session_activity(
        &self,
        _session_id: &str,
        _user_id: &str,
        _plan: SessionActivityUpdatePlan,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnCoreEventWriter;

#[async_trait]
impl TurnCoreEventWriter for NoopTurnCoreEventWriter {
    async fn persist(&self, plan: TurnCorePersistPlan) -> Result<TurnCorePersistOutcome, String> {
        Ok(TurnCorePersistOutcome {
            llm_response_event_id: plan.llm_response_event.map(|event| event.event_id),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnToolEventWriter;

#[async_trait]
impl TurnToolEventWriter for NoopTurnToolEventWriter {
    async fn persist(&self, _plan: TurnToolEventPersistPlan) -> Result<(), String> {
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

    #[test]
    fn tool_event_writer_accepts_only_canonical_lifecycle_types() {
        for event_type in [
            "tool_call_started",
            "tool_call_completed",
            "tool_call_failed",
            "tool_call_rejected",
            "tool_call_reused",
            "tool_call_suppressed",
            "tool_call_deferred",
        ] {
            assert!(validate_tool_lifecycle_event_type(event_type).is_ok());
        }
        for event_type in ["tool_call", "tool_result", "tool_error", ""] {
            assert!(validate_tool_lifecycle_event_type(event_type).is_err());
        }
    }

    #[test]
    fn bridge_transcript_projection_excludes_runtime_envelope_but_keeps_reply() {
        let user = core_event(
            "user-event",
            "user-1",
            "session-1",
            "chain-1",
            "user_query",
            "real user input",
            None,
        );
        let runtime = core_event(
            "runtime-event",
            "user-1",
            "session-1",
            "chain-2",
            "runtime_reconciliation",
            astra_turn_core::chat_turn_edge_profile::RUNTIME_RECONCILIATION_USER_ENVELOPE,
            None,
        );
        let response = core_event(
            "response-event",
            "user-1",
            "session-1",
            "chain-2",
            "llm_response",
            "reconciled result",
            Some("runtime-event"),
        );

        let user_item = bridge_transcript_item(&user).expect("human input transcript item");
        assert_eq!(user_item.role, "user");
        assert_eq!(user_item.content, "real user input");
        assert!(user_item.run_id.is_none());
        assert!(bridge_transcript_item(&runtime).is_none());
        let response_item =
            bridge_transcript_item(&response).expect("runtime reply transcript item");
        assert_eq!(response_item.role, "assistant");
        assert_eq!(response_item.content, "reconciled result");
    }

    #[test]
    fn bridge_transcript_projection_skips_blank_tool_only_model_rounds() {
        let response = core_event(
            "response-event",
            "user-1",
            "session-1",
            "chain-1",
            "llm_response",
            "  ",
            Some("user-event"),
        );

        assert!(bridge_transcript_item(&response).is_none());
    }

    fn core_event(
        event_id: &str,
        user_id: &str,
        session_id: &str,
        causal_chain_id: &str,
        event_type: &str,
        content: &str,
        parent_event_id: Option<&str>,
    ) -> TurnCoreEventRecord {
        TurnCoreEventRecord {
            event_id: event_id.to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            run_id: None,
            agent_id: None,
            event_type: event_type.to_string(),
            content: content.to_string(),
            parent_event_id: parent_event_id.map(str::to_string),
            parent_event_ids: parent_event_id
                .map(|id| vec![id.to_string()])
                .unwrap_or_default(),
            causal_chain_id: causal_chain_id.to_string(),
            turn_seq: Some(1),
            llm_model_used: None,
            token_usage: None,
            llm_params: None,
            reasoning_content: None,
        }
    }

    fn tool_event(
        event_id: &str,
        user_id: &str,
        session_id: &str,
        causal_chain_id: &str,
        event_type: &str,
        content: &str,
        parent_event_id: Option<&str>,
    ) -> TurnToolEventRecord {
        TurnToolEventRecord {
            event_id: event_id.to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            run_id: None,
            tool_call_id: None,
            agent_id: None,
            event_type: event_type.to_string(),
            content: content.to_string(),
            parent_event_id: parent_event_id.map(str::to_string),
            parent_event_ids: parent_event_id
                .map(|id| vec![id.to_string()])
                .unwrap_or_default(),
            causal_chain_id: causal_chain_id.to_string(),
            metadata: None,
            skill_name: None,
            skill_version: None,
            reasoning_content: None,
        }
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
    async fn turn_event_writers_increment_event_count_by_insert_delta_on_live_matrixone() {
        let shared = setup_live_pool_for_test().await;
        let pool = shared.get().clone();
        let settings = MatrixOneSettings::from_env();
        let suffix = Uuid::new_v4().to_string();
        let session_id = format!("turn-writer-{suffix}");
        let user_id = format!("user-{suffix}");
        let causal_chain_id = format!("chain-{suffix}");
        let core_user_event_id = format!("core-user-{suffix}");
        let core_response_event_id = format!("core-response-{suffix}");
        let tool_duplicate_event_id = format!("tool-dup-{suffix}");
        let tool_unique_event_id = format!("tool-unique-{suffix}");
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

        let core_writer =
            DatabaseTurnCoreEventWriter::new(settings.clone()).with_pool(shared.clone());
        core_writer
            .persist(TurnCorePersistPlan {
                user_query_event: Some(core_event(
                    &core_user_event_id,
                    &user_id,
                    &session_id,
                    &causal_chain_id,
                    "user_query",
                    "hello",
                    None,
                )),
                llm_response_event: Some(core_event(
                    &core_response_event_id,
                    &user_id,
                    &session_id,
                    &causal_chain_id,
                    "llm_response",
                    "world",
                    Some(&core_user_event_id),
                )),
                snapshot_link_plan: None,
            })
            .await
            .expect("persist core events");
        core_writer
            .persist(TurnCorePersistPlan {
                user_query_event: Some(core_event(
                    &core_user_event_id,
                    &user_id,
                    &session_id,
                    &causal_chain_id,
                    "user_query",
                    "duplicate",
                    None,
                )),
                llm_response_event: None,
                snapshot_link_plan: None,
            })
            .await
            .expect("persist duplicate core event");

        let tool_writer =
            DatabaseTurnToolEventWriter::new(settings.clone()).with_pool(shared.clone());
        tool_writer
            .persist(TurnToolEventPersistPlan {
                events: vec![
                    tool_event(
                        &tool_duplicate_event_id,
                        &user_id,
                        &session_id,
                        &causal_chain_id,
                        "tool_call_started",
                        "first duplicate",
                        Some(&core_response_event_id),
                    ),
                    tool_event(
                        &tool_duplicate_event_id,
                        &user_id,
                        &session_id,
                        &causal_chain_id,
                        "tool_call_started",
                        "second duplicate",
                        Some(&core_response_event_id),
                    ),
                    tool_event(
                        &tool_unique_event_id,
                        &user_id,
                        &session_id,
                        &causal_chain_id,
                        "tool_call_completed",
                        "unique",
                        Some(&tool_duplicate_event_id),
                    ),
                ],
            })
            .await
            .expect("persist tool events");

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
                    Some(&tool_unique_event_id),
                ),
                auxiliary_event(
                    &aux_duplicate_event_id,
                    &user_id,
                    &session_id,
                    &causal_chain_id,
                    "system_note",
                    "second duplicate",
                    Some(&tool_unique_event_id),
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

        DatabaseTurnSessionActivityWriter::new(settings)
            .with_pool(shared.clone())
            .update_session_activity(
                &session_id,
                &user_id,
                SessionActivityUpdatePlan {
                    last_event_id: Some(aux_unique_event_id.clone()),
                },
            )
            .await
            .expect("touch session activity");

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
            6,
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
        assert_eq!(actual_events, 6);

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

    /// Verify that all Database*Writer structs fail instantly when no pool is
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

        // CoreEventWriter
        let w = DatabaseTurnCoreEventWriter::new(settings.clone());
        let r = w
            .persist(TurnCorePersistPlan {
                user_query_event: Some(TurnCoreEventRecord {
                    event_id: "e1".into(),
                    user_id: "u".into(),
                    session_id: "s".into(),
                    run_id: None,
                    agent_id: None,
                    event_type: "user_query".into(),
                    content: "hi".into(),
                    parent_event_id: None,
                    parent_event_ids: vec![],
                    causal_chain_id: "c".into(),
                    turn_seq: Some(1),
                    llm_model_used: None,
                    token_usage: None,
                    llm_params: None,
                    reasoning_content: None,
                }),
                llm_response_event: None,
                snapshot_link_plan: None,
            })
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("not configured"));

        // HookDbWriter
        let w = DatabaseTurnHookDbWriter::new(settings.clone());
        let r = w
            .persist(TurnHookDbPersistPlan {
                decision_audit: Some(TurnDecisionAuditRecord {
                    decision_id: "d1".into(),
                    user_id: "u".into(),
                    event_id: "e2".into(),
                    session_id: "s".into(),
                    decision_type: "tool_surface".into(),
                    decision_output: json!({}),
                    model_used: None,
                    context_capture_id: None,
                }),
                skill_selection: None,
                reflection_lesson: None,
                reflection_mark: None,
            })
            .await;
        assert!(r.is_err());

        // ToolEventWriter
        let w = DatabaseTurnToolEventWriter::new(settings.clone());
        let r = w
            .persist(TurnToolEventPersistPlan {
                events: vec![TurnToolEventRecord {
                    event_id: "e3".into(),
                    user_id: "u".into(),
                    session_id: "s".into(),
                    run_id: None,
                    tool_call_id: None,
                    agent_id: None,
                    event_type: "tool_call_started".into(),
                    content: "x".into(),
                    parent_event_id: None,
                    parent_event_ids: vec![],
                    causal_chain_id: "c".into(),
                    metadata: None,
                    skill_name: None,
                    skill_version: None,
                    reasoning_content: None,
                }],
            })
            .await;
        assert!(r.is_err());

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

        // SessionActivityWriter
        let w = DatabaseTurnSessionActivityWriter::new(settings);
        let r = w
            .update_session_activity(
                "s",
                "u",
                SessionActivityUpdatePlan {
                    last_event_id: Some("e5".into()),
                },
            )
            .await;
        assert!(r.is_err());

        // All 5 must complete in <100ms (previously each took 2s)
        assert!(
            start.elapsed().as_millis() < 100,
            "no-pool writers took {}ms — should be instant",
            start.elapsed().as_millis()
        );
    }
}
