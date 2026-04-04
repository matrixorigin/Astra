use crate::*;

#[derive(Clone, Debug)]
pub struct DatabaseTurnSessionActivityWriter {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnToolEventWriter {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnHookDbWriter {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnAuxiliaryEventWriter {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug)]
pub struct DatabaseTurnCoreEventWriter {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseTurnSessionActivityWriter {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

impl DatabaseTurnToolEventWriter {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

impl DatabaseTurnHookDbWriter {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

impl DatabaseTurnAuxiliaryEventWriter {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

impl DatabaseTurnCoreEventWriter {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
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
        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
        let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
        if let Some(event) = plan.user_query_event.as_ref() {
            insert_core_turn_event(&mut tx, event)
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Some(event) = plan.llm_response_event.as_ref() {
            insert_core_turn_event(&mut tx, event)
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
        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
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
        for event in &plan.events {
            insert_tool_turn_event(
                &mut tx,
                event,
                skill_versions.get(event.skill_name.as_deref().unwrap_or("")),
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[async_trait]
impl TurnHookDbWriter for DatabaseTurnHookDbWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        if plan.decision_audit.is_none()
            && plan.skill_selection.is_none()
            && plan.implicit_feedback.is_none()
        {
            return Ok(());
        }
        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
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
        if let Some(implicit_feedback) = plan.implicit_feedback.as_ref() {
            insert_turn_implicit_feedback(&mut tx, implicit_feedback)
                .await
                .map_err(|error| error.to_string())?;
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
        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
        let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
        for event in events {
            let meta_tool_name = event
                .metadata
                .as_ref()
                .and_then(|v| v.get("tool_name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let meta_duration_ms = event
                .metadata
                .as_ref()
                .and_then(|v| v.get("duration_ms"))
                .and_then(|v| v.as_i64())
                .map(|v| v as i32);
            let metadata_json = event.metadata.map(|metadata| metadata.to_string());
            query(
                "INSERT INTO agent_events \
                 (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
                  parent_event_id, causal_chain_id, `metadata`, reasoning_content, \
                  meta_tool_name, meta_duration_ms, created_at) \
                 VALUES (?, ?, ?, 'dev-agent', '0.1.0', ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
            )
            .bind(event.event_id)
            .bind(event.session_id)
            .bind(event.user_id)
            .bind(event.event_type)
            .bind(event.content)
            .bind(event.parent_event_id)
            .bind(event.causal_chain_id)
            .bind(metadata_json)
            .bind(event.reasoning_content)
            .bind(meta_tool_name)
            .bind(meta_duration_ms)
            .execute(&mut *tx)
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
        plan: SessionActivityUpdatePlan,
    ) -> Result<(), String> {
        let pool = self.get_pool().await.map_err(|error| error.to_string())?;
        let result = query(
            "UPDATE agent_sessions \
             SET event_count = event_count + ?, last_active_at = NOW(), updated_at = NOW(), \
                 last_event_id = COALESCE(?, last_event_id) \
             WHERE session_id = ?",
        )
        .bind(plan.event_count_increment as i64)
        .bind(plan.last_event_id)
        .bind(session_id)
        .execute(&pool)
        .await
        .map_err(|error| error.to_string());
        result.map(|_| ())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnSessionActivityWriter;

#[async_trait]
impl TurnSessionActivityWriter for NoopTurnSessionActivityWriter {
    async fn update_session_activity(
        &self,
        _session_id: &str,
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
