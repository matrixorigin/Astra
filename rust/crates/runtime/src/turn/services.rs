use crate::data_layer::storage::{
    bump_agent_session_event_count, insert_trace_event, touch_agent_session_activity,
};
use crate::*;
use astra_turn_core::trace_event::{TraceEvent, TraceEventWriter, TraceWriteError};

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

fn metadata_tool_name(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata
        .and_then(|v| v.get("tool_name").or_else(|| v.get("name")))
        .and_then(|v| v.as_str())
        .map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
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
        let payload = serde_json::json!({
            "messages": request.messages,
            "session_id": request.session_id,
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?
            .post(format!(
                "{}/v1/observe",
                self.base_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {master_key}"))
            .header("X-Impersonate-User", request.user_id)
            .json(&payload)
            .send()
            .await
            .map_err(|error| error.to_string())?;
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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_tool_name_none() {
        assert!(metadata_tool_name(None).is_none());
    }

    #[test]
    fn metadata_tool_name_from_tool_name() {
        let v = json!({"tool_name": "bash"});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "bash");
    }

    #[test]
    fn metadata_tool_name_from_name_fallback() {
        let v = json!({"name": "read_file"});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "read_file");
    }

    #[test]
    fn metadata_tool_name_prefers_tool_name() {
        let v = json!({"tool_name": "preferred", "name": "fallback"});
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
    fn turn_event_writers_use_delta_updates_not_count_reconcile() {
        let source = include_str!("services.rs");
        let forbidden_load = concat!("load_agent_event_count", "_for_user");
        let forbidden_upsert = concat!("upsert_agent_session", "_event_count");
        let forbidden_subquery = concat!("event_count = (SELECT ", "COUNT(*)");
        assert!(
            source.contains("bump_agent_session_event_count"),
            "event writers must maintain agent_sessions.event_count with actual insert deltas"
        );
        assert!(
            source.contains("touch_agent_session_activity"),
            "activity writer should only touch activity metadata"
        );
        assert!(
            !source.contains(forbidden_load),
            "turn event writer hot path must not COUNT(*) agent_events"
        );
        assert!(
            !source.contains(forbidden_upsert),
            "turn event writer hot path must not reconcile event_count from COUNT(*)"
        );
        assert!(
            !source.contains(forbidden_subquery),
            "turn activity update must not embed a COUNT(*) subquery"
        );
    }

    #[test]
    fn metadata_tool_name_non_string_value() {
        let v = json!({"tool_name": 42});
        assert!(metadata_tool_name(Some(&v)).is_none());
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
                    agent_id: None,
                    event_type: "tool_call".into(),
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
