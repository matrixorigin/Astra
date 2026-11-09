//! Run settings share one journal snapshot and take effect at a round boundary.
use super::*;
use astra_turn_types::{
    ModelSelector, ReasoningRequirement, RunModelOutcome, RunModelOutcomeKind, RunModelRequest,
    RunModelSelection,
};

pub(super) const SETTINGS: &str = "run_settings_snapshot";

/// The last admitted model is retained separately from the latest outcome:
/// a rejected later request must not erase the effective model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunModelApplied {
    pub selection: RunModelSelection,
    pub resolved: ResolvedModelSelection,
    pub reasoning: ReasoningRequirement,
    pub owner_generation: u64,
    pub round_index: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunModelSettingsSnapshot {
    pub requested: Option<RunModelSelection>,
    pub outcome: Option<RunModelOutcome>,
    pub applied: Option<RunModelApplied>,
}

impl RunModelSettingsSnapshot {
    pub fn client_snapshot(&self) -> astra_turn_types::RunModelSnapshot {
        astra_turn_types::RunModelSnapshot {
            requested: self.requested.clone(),
            outcome: self.outcome.clone(),
            applied: self.applied.as_ref().map(|applied| RunModelOutcome {
                selection: applied.selection.clone(),
                owner_generation: applied.owner_generation,
                round_index: applied.round_index,
                result: RunModelOutcomeKind::Applied {
                    model: applied.selection.model.clone(),
                    reasoning: applied.reasoning.clone(),
                },
            }),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSettingsSnapshot {
    pub permission: RunPermissionModeSnapshot,
    pub model: RunModelSettingsSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(super) enum RunSettingsChange {
    PermissionRequested(RunPermissionModeSelection),
    PermissionApplied(RunPermissionModeApplied),
    ModelRequested(RunModelSelection),
    ModelOutcome(RunModelOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RunSettingsEvent {
    pub change: RunSettingsChange,
    pub snapshot: RunSettingsSnapshot,
    pub run_id: String,
    pub session_id: String,
}

fn read_settings_event(event: &serde_json::Value) -> Result<RunSettingsEvent, String> {
    serde_json::from_value(event.get("data").cloned().ok_or("missing run settings")?)
        .map_err(|error| format!("invalid run settings: {error}"))
}

pub(super) fn read_settings_snapshot(payload: &str) -> Result<RunSettingsSnapshot, String> {
    let event = serde_json::from_str(payload).map_err(|error| error.to_string())?;
    Ok(read_settings_event(&event)?.snapshot)
}

fn settings_event(
    change: RunSettingsChange,
    snapshot: RunSettingsSnapshot,
    key: String,
    run_id: &str,
    session_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "event_type": SETTINGS,
        "idempotency_key": key,
        "data": RunSettingsEvent {change, snapshot, run_id: run_id.into(), session_id: session_id.into()}
    })
}

#[derive(Clone, Copy)]
enum SettingsRequest<'a> {
    Permission(&'a RunPermissionModeRequest),
    Model(&'a RunModelRequest),
}

impl<'a> SettingsRequest<'a> {
    fn identity(self) -> (&'a str, &'a str, &'static str) {
        match self {
            Self::Permission(request) => (
                &request.expected_session_id,
                &request.request_id,
                "permission",
            ),
            Self::Model(request) => (&request.expected_session_id, &request.request_id, "model"),
        }
    }

    fn validate(self) -> Result<(), String> {
        let (session_id, request_id, _) = self.identity();
        if session_id.trim().is_empty()
            || request_id.trim().is_empty()
            || request_id.len() > 128
            || request_id.trim() != request_id
        {
            return Err("run setting requires a session and a nonempty request_id of at most 128 bytes without surrounding whitespace".into());
        }
        if let Self::Model(request) = self {
            ModelSelector::OfferingId {
                offering_id: request.model.offering_id.clone(),
            }
            .validate()
            .map_err(str::to_string)?;
            if matches!(
                request.reasoning,
                ReasoningRequirement::Budget { tokens: 0 }
            ) {
                return Err("reasoning budget must be positive".into());
            }
        }
        Ok(())
    }

    fn key(self) -> String {
        let (_, request_id, kind) = self.identity();
        format!(
            "run-settings:{kind}-request:{}",
            sha256_hex(request_id.as_bytes())
        )
    }

    fn change(self, revision: i64) -> RunSettingsChange {
        match self {
            Self::Permission(request) => {
                RunSettingsChange::PermissionRequested(RunPermissionModeSelection {
                    request_id: request.request_id.clone(),
                    mode: request.mode,
                    revision,
                })
            }
            Self::Model(request) => RunSettingsChange::ModelRequested(RunModelSelection {
                request_id: request.request_id.clone(),
                model: request.model.clone(),
                reasoning: request.reasoning.clone(),
                revision,
            }),
        }
    }

    fn update_snapshot(self, snapshot: &mut RunSettingsSnapshot, change: &RunSettingsChange) {
        match (self, change) {
            (Self::Permission(_), RunSettingsChange::PermissionRequested(selected)) => {
                snapshot.permission.requested = Some(selected.clone())
            }
            (Self::Model(_), RunSettingsChange::ModelRequested(selected)) => {
                snapshot.model.requested = Some(selected.clone())
            }
            _ => unreachable!("request and generated setting must match"),
        }
    }
}

fn permission_selection(event: &serde_json::Value) -> Result<RunPermissionModeSelection, String> {
    match read_settings_event(event)?.change {
        RunSettingsChange::PermissionRequested(selection) => Ok(selection),
        _ => Err("run setting is not a permission request".into()),
    }
}

fn model_selection(event: &serde_json::Value) -> Result<RunModelSelection, String> {
    match read_settings_event(event)?.change {
        RunSettingsChange::ModelRequested(selection) => Ok(selection),
        _ => Err("run setting is not a model request".into()),
    }
}

fn model_outcome_key(revision: i64) -> String {
    format!("run-settings:model-outcome:{revision}")
}

fn finish_model_decision(
    mut snapshot: RunSettingsSnapshot,
    mut outcome: RunModelOutcome,
    resolved: Option<&ResolvedModelSelection>,
) -> Result<(RunSettingsSnapshot, RunModelOutcome), String> {
    if snapshot.model.requested.as_ref() != Some(&outcome.selection) {
        outcome.result = RunModelOutcomeKind::Superseded;
        return Ok((snapshot, outcome));
    }
    match &outcome.result {
        RunModelOutcomeKind::Applied { model, reasoning } => {
            let resolved = resolved.ok_or("applied model lacks its admitted identity")?;
            if model != &outcome.selection.model
                || reasoning != &outcome.selection.reasoning
                || resolved.offering_id != model.offering_id
            {
                return Err("applied model does not match the requested Offering".into());
            }
            snapshot.model.applied = Some(RunModelApplied {
                selection: outcome.selection.clone(),
                resolved: resolved.clone(),
                reasoning: reasoning.clone(),
                owner_generation: outcome.owner_generation,
                round_index: outcome.round_index,
            });
        }
        RunModelOutcomeKind::Rejected { error_code, .. } => {
            if resolved.is_some()
                || error_code.as_ref().is_some_and(|code| {
                    code.is_empty()
                        || code.len() > 128
                        || !code
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                })
            {
                return Err("invalid model rejection".into());
            }
        }
        RunModelOutcomeKind::Superseded => {
            return Err("model supersession is determined by the run owner".into());
        }
    }
    snapshot.model.outcome = Some(outcome.clone());
    Ok((snapshot, outcome))
}

fn snapshot_from_events(events: &[serde_json::Value]) -> Result<RunSettingsSnapshot, String> {
    events
        .iter()
        .rev()
        .find(|e| extract_event_type(e) == SETTINGS)
        .map(read_settings_event)
        .transpose()
        .map(|event| event.map(|e| e.snapshot).unwrap_or_default())
}

fn application_event(
    selection: &RunPermissionModeSelection,
    generation: u64,
    round: u32,
    run_id: &str,
    session_id: &str,
    mut snapshot: RunSettingsSnapshot,
) -> serde_json::Value {
    let applied = RunPermissionModeApplied {
        selection: selection.clone(),
        owner_generation: generation,
        round_index: round,
    };
    snapshot.permission.applied = Some(applied.clone());
    settings_event(
        RunSettingsChange::PermissionApplied(applied),
        snapshot,
        format!(
            "run-settings:permission-applied:{generation}:{round}:{}",
            selection.revision
        ),
        run_id,
        session_id,
    )
}

impl InMemoryRunStateStore {
    pub(super) async fn request_permission_mode_inner(
        &self,
        user_id: &str,
        run_id: &str,
        request: &RunPermissionModeRequest,
    ) -> Result<RunPermissionModeSelection, String> {
        match self
            .request_settings_inner(user_id, run_id, SettingsRequest::Permission(request))
            .await?
        {
            RunSettingsChange::PermissionRequested(selected) => Ok(selected),
            _ => unreachable!("permission request produced a different setting"),
        }
    }

    pub(super) async fn request_model_inner(
        &self,
        user_id: &str,
        run_id: &str,
        request: &RunModelRequest,
    ) -> Result<RunModelSelection, String> {
        match self
            .request_settings_inner(user_id, run_id, SettingsRequest::Model(request))
            .await?
        {
            RunSettingsChange::ModelRequested(selected) => Ok(selected),
            _ => unreachable!("model request produced a different setting"),
        }
    }

    async fn request_settings_inner(
        &self,
        user_id: &str,
        run_id: &str,
        request: SettingsRequest<'_>,
    ) -> Result<RunSettingsChange, String> {
        request.validate()?;
        let (session_id, _, _) = request.identity();
        let fence = self.action_fence_for(user_id, run_id);
        let _guard = fence.lock_owned().await;
        let cancellations = self.cancellation_requests.read().await;
        let mut runs = self.runs.write().await;
        let run = runs
            .get_mut(run_id)
            .filter(|r| r.user_id == user_id && r.session_id == session_id && r.depth == 0)
            .ok_or("run setting target not found")?;
        let key = request.key();
        if let Some((idx, existing)) = run
            .events
            .iter()
            .enumerate()
            .find(|(_, e)| e["idempotency_key"] == key)
        {
            let change = read_settings_event(existing)?.change;
            if change != request.change(idx as i64) {
                return Err("run setting request identity conflict".into());
            }
            return Ok(change);
        }
        if !matches!(run.status.as_str(), STATUS_RUNNING | STATUS_WAITING)
            || cancellations.contains(&(user_id.to_string(), run_id.to_string()))
        {
            return Err("run setting cannot be changed after the run is inactive".into());
        }
        let change = request.change(run.events.len() as i64);
        let mut snapshot = snapshot_from_events(&run.events)?;
        request.update_snapshot(&mut snapshot, &change);
        let event = settings_event(change.clone(), snapshot, key, run_id, session_id);
        run.events.push(event);
        run.last_event_idx = run.events.len() as i64 - 1;
        run.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(change)
    }

    pub(super) async fn run_settings_snapshot_inner(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<RunSettingsSnapshot>, String> {
        let runs = self.runs.read().await;
        runs.get(run_id)
            .filter(|r| r.user_id == user_id && r.session_id == session_id && r.depth == 0)
            .map(|r| snapshot_from_events(&r.events))
            .transpose()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn apply_permission_mode_inner(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        generation: u64,
        selected: &RunPermissionModeSelection,
        round: u32,
    ) -> Result<bool, String> {
        let fence = self.action_fence_for(user_id, run_id);
        let _guard = fence.lock_owned().await;
        let cancellations = self.cancellation_requests.read().await;
        let mut runs = self.runs.write().await;
        let Some(run) = runs
            .get_mut(run_id)
            .filter(|r| r.user_id == user_id && r.session_id == session_id && r.depth == 0)
        else {
            return Ok(false);
        };
        if run.run_generation != generation
            || run.status != STATUS_RUNNING
            || cancellations.contains(&(user_id.to_string(), run_id.to_string()))
            || run.owner_pod_id.as_deref() != self.execution_owner_pod_id.as_deref()
            || !in_memory_action_owner_lease_is_active(run)?
        {
            return Ok(false);
        }
        let Some(event) = usize::try_from(selected.revision)
            .ok()
            .and_then(|i| run.events.get(i))
        else {
            return Ok(false);
        };
        if extract_event_type(event) != SETTINGS
            || permission_selection(event).ok().as_ref() != Some(selected)
        {
            return Ok(false);
        }
        let snapshot = snapshot_from_events(&run.events)?;
        if snapshot.permission.applied.as_ref().is_some_and(|a| {
            a.selection.revision > selected.revision
                || (a.owner_generation == generation && a.round_index > round)
        }) {
            return Ok(false);
        }
        let event = application_event(selected, generation, round, run_id, session_id, snapshot);
        if let Some(existing) = run
            .events
            .iter()
            .find(|e| e["idempotency_key"] == event["idempotency_key"])
        {
            return Ok(
                matches!(read_settings_event(existing)?.change, RunSettingsChange::PermissionApplied(applied)
                if applied.selection == *selected && applied.owner_generation == generation && applied.round_index == round),
            );
        }
        run.events.push(event);
        run.last_event_idx = run.events.len() as i64 - 1;
        run.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(true)
    }

    pub(super) async fn finish_model_request_inner(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        outcome: &RunModelOutcome,
        resolved: Option<&ResolvedModelSelection>,
    ) -> Result<Option<RunModelOutcome>, String> {
        let fence = self.action_fence_for(user_id, run_id);
        let _guard = fence.lock_owned().await;
        let cancellations = self.cancellation_requests.read().await;
        let mut runs = self.runs.write().await;
        let Some(run) = runs
            .get_mut(run_id)
            .filter(|r| r.user_id == user_id && r.session_id == session_id && r.depth == 0)
        else {
            return Ok(None);
        };
        if run.run_generation != outcome.owner_generation
            || run.status != STATUS_RUNNING
            || cancellations.contains(&(user_id.to_string(), run_id.to_string()))
            || run.owner_pod_id.as_deref() != self.execution_owner_pod_id.as_deref()
            || !in_memory_action_owner_lease_is_active(run)?
        {
            return Ok(None);
        }
        let Some(source) = usize::try_from(outcome.selection.revision)
            .ok()
            .and_then(|idx| run.events.get(idx))
        else {
            return Ok(None);
        };
        if extract_event_type(source) != SETTINGS
            || model_selection(source).ok().as_ref() != Some(&outcome.selection)
        {
            return Ok(None);
        }
        let key = model_outcome_key(outcome.selection.revision);
        if let Some(existing) = run
            .events
            .iter()
            .find(|event| event["idempotency_key"] == key)
        {
            return match read_settings_event(existing)?.change {
                RunSettingsChange::ModelOutcome(previous)
                    if previous.selection == outcome.selection =>
                {
                    Ok(Some(previous))
                }
                _ => Err("model outcome identity conflict".into()),
            };
        }
        let snapshot = snapshot_from_events(&run.events)?;
        let (snapshot, decided) = finish_model_decision(snapshot, outcome.clone(), resolved)?;
        if matches!(decided.result, RunModelOutcomeKind::Applied { .. }) {
            let applied = snapshot
                .model
                .applied
                .as_ref()
                .expect("applied outcome has admitted identity");
            run.model_offering_id = Some(applied.resolved.offering_id.clone());
            run.resolved_model_name = Some(applied.resolved.model_name.clone());
        }
        run.events.push(settings_event(
            RunSettingsChange::ModelOutcome(decided.clone()),
            snapshot,
            key,
            run_id,
            session_id,
        ));
        run.last_event_idx = run.events.len() as i64 - 1;
        run.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(Some(decided))
    }
}

impl DatabaseRunStateStore {
    async fn settings_snapshot_tx(
        tx: &mut sqlx::Transaction<'_, MySql>,
        user_id: &str,
        run_id: &str,
    ) -> Result<RunSettingsSnapshot, String> {
        // Use a current read after taking the run fence. A transaction which
        // waited for another writer must not merge an earlier MVCC snapshot.
        let payload: Option<String> = sqlx::query_scalar("SELECT payload_json FROM agent_run_events WHERE user_id=? AND run_id=? AND event_type=? ORDER BY event_idx DESC LIMIT 1 FOR UPDATE")
            .bind(user_id).bind(run_id).bind(SETTINGS).fetch_optional(&mut **tx).await.map_err(|e| e.to_string())?;
        payload
            .as_deref()
            .map(read_settings_snapshot)
            .transpose()
            .map(|snapshot| snapshot.unwrap_or_default())
    }

    async fn settings_snapshot_row(
        pool: &sqlx::Pool<MySql>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<RunSettingsSnapshot>, String> {
        let row = sqlx::query(
            "SELECT settings.payload_json AS settings_payload \
             FROM agent_runs AS run \
             LEFT JOIN (SELECT payload_json FROM agent_run_events \
             WHERE user_id=? AND run_id=? AND event_type=? ORDER BY event_idx DESC LIMIT 1) AS settings ON TRUE \
             WHERE run.user_id=? AND run.session_id=? AND run.run_id=? AND run.depth=0 LIMIT 1",
        )
        .bind(user_id)
        .bind(run_id)
        .bind(SETTINGS)
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
        let Some(row) = row else {
            return Ok(None);
        };
        let payload: Option<String> = row.try_get("settings_payload").map_err(|e| e.to_string())?;
        let snapshot = payload
            .as_deref()
            .map(read_settings_snapshot)
            .transpose()?
            .unwrap_or_default();
        Ok(Some(snapshot))
    }

    async fn append_settings_event_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        run: &DurableRunRecord,
        event: &serde_json::Value,
        expected_generation: Option<u64>,
        effective_model: Option<&ResolvedModelSelection>,
    ) -> Result<i64, String> {
        let idx = run.last_event_idx + 1;
        let row = build_run_event_insert_row(
            &run.user_id,
            &run.run_id,
            &run.session_id,
            run.agent_id.as_deref(),
            idx,
            &self.owner_pod_id,
            event,
        )
        .map_err(|e| e.to_string())?;
        let mut update = sqlx::QueryBuilder::<MySql>::new("UPDATE agent_runs SET last_event_idx=");
        update.push_bind(idx).push(",updated_at=NOW(6)");
        if let Some(model) = effective_model {
            update
                .push(",model_offering_id=")
                .push_bind(&model.offering_id)
                .push(",resolved_model_name=")
                .push_bind(&model.model_name);
        }
        update
            .push(" WHERE user_id=")
            .push_bind(&run.user_id)
            .push(" AND session_id=")
            .push_bind(&run.session_id)
            .push(" AND run_id=")
            .push_bind(&run.run_id)
            .push(" AND last_event_idx=")
            .push_bind(run.last_event_idx);
        if let Some(generation) = expected_generation {
            update.push(" AND run_generation=").push_bind(i64::try_from(generation).map_err(|e|e.to_string())?)
                .push(" AND owner_pod_id=").push_bind(&self.owner_pod_id)
                .push(" AND owner_lease_expires_at>=NOW(6) AND cancellation_requested_at IS NULL AND status='running'");
        }
        let updated = update
            .build()
            .execute(&mut **tx)
            .await
            .map_err(|e| e.to_string())?;
        if updated.rows_affected() != 1 {
            return Err("run setting append lost run authority".into());
        }
        Self::insert_run_event_rows_tx(tx, &run.run_id, &[row], "run_settings_append")
            .await
            .map_err(|error| error.to_string())?;
        Ok(idx)
    }

    pub(super) async fn request_permission_mode_inner(
        &self,
        user_id: &str,
        run_id: &str,
        request: &RunPermissionModeRequest,
    ) -> Result<RunPermissionModeSelection, String> {
        match self
            .request_settings_inner(user_id, run_id, SettingsRequest::Permission(request))
            .await?
        {
            RunSettingsChange::PermissionRequested(selected) => Ok(selected),
            _ => unreachable!("permission request produced a different setting"),
        }
    }

    pub(super) async fn request_model_inner(
        &self,
        user_id: &str,
        run_id: &str,
        request: &RunModelRequest,
    ) -> Result<RunModelSelection, String> {
        match self
            .request_settings_inner(user_id, run_id, SettingsRequest::Model(request))
            .await?
        {
            RunSettingsChange::ModelRequested(selected) => Ok(selected),
            _ => unreachable!("model request produced a different setting"),
        }
    }

    async fn request_settings_inner(
        &self,
        user_id: &str,
        run_id: &str,
        request: SettingsRequest<'_>,
    ) -> Result<RunSettingsChange, String> {
        request.validate()?;
        let (session_id, _, _) = request.identity();
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|e| e.to_string())?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|e| e.to_string())?;
        let run = self
            .load_run_metadata_for_exact_session_tx(
                &mut tx,
                user_id,
                session_id,
                run_id,
                RunProjection::Opening,
            )
            .await
            .map_err(|e| e.to_string())?
            .filter(|r| r.depth == 0)
            .ok_or("run setting target not found")?;
        let key = request.key();
        let row=sqlx::query("SELECT event_idx,payload_json FROM agent_run_events WHERE user_id=? AND run_id=? AND idempotency_key=? LIMIT 1 FOR UPDATE")
            .bind(user_id).bind(run_id).bind(&key).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
        if let Some(row) = row {
            let idx: i64 = row.try_get("event_idx").map_err(|e| e.to_string())?;
            let payload: String = row.try_get("payload_json").map_err(|e| e.to_string())?;
            let existing: serde_json::Value =
                serde_json::from_str(&payload).map_err(|e| e.to_string())?;
            let change = read_settings_event(&existing)?.change;
            if change != request.change(idx) {
                return Err("run setting request identity conflict".into());
            }
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(change);
        }
        if !matches!(run.status.as_str(), STATUS_RUNNING | STATUS_WAITING) {
            return Err("run setting cannot be changed after the run is inactive".into());
        }
        if lock_durable_lineage_cancellation_markers_tx(&mut tx, &run)
            .await?
            .any()
        {
            return Err("run setting target is cancelled".into());
        }
        let change = request.change(run.last_event_idx + 1);
        let mut snapshot = Self::settings_snapshot_tx(&mut tx, user_id, run_id).await?;
        request.update_snapshot(&mut snapshot, &change);
        let event = settings_event(change.clone(), snapshot, key, run_id, session_id);
        self.append_settings_event_tx(&mut tx, &run, &event, None, None)
            .await?;
        tx.commit()
            .await
            .map_err(|e| format!("run setting commit unconfirmed; retry same request_id: {e}"))?;
        connection.release();
        Ok(change)
    }

    pub(super) async fn run_settings_snapshot_inner(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<RunSettingsSnapshot>, String> {
        // This hot round-boundary lookup is read-only. It must not fetch
        // checkpoint_json, hydrate run history, or take session/run write
        // fences. A request committed before this point is observed; one
        // committed after it waits for the next round boundary.
        Self::settings_snapshot_row(self.pool.get(), user_id, session_id, run_id).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn apply_permission_mode_inner(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        generation: u64,
        selected: &RunPermissionModeSelection,
        round: u32,
    ) -> Result<bool, String> {
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|e| e.to_string())?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|e| e.to_string())?;
        let Some(run) = self
            .load_run_metadata_for_exact_session_tx(
                &mut tx,
                user_id,
                session_id,
                run_id,
                RunProjection::Opening,
            )
            .await
            .map_err(|e| e.to_string())?
        else {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(false);
        };
        if run.depth != 0
            || run.run_generation != generation
            || run.status != STATUS_RUNNING
            || run.owner_pod_id.as_deref() != Some(self.owner_pod_id.as_str())
        {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(false);
        }
        if lock_durable_lineage_cancellation_markers_tx(&mut tx, &run)
            .await?
            .any()
        {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(false);
        }
        let live:Option<i32>=sqlx::query_scalar("SELECT 1 FROM agent_runs WHERE user_id=? AND run_id=? AND owner_pod_id=? AND run_generation=? AND owner_lease_expires_at>=NOW(6) FOR UPDATE")
            .bind(user_id).bind(run_id).bind(&self.owner_pod_id).bind(i64::try_from(generation).map_err(|e|e.to_string())?).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
        if live.is_none() {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(false);
        }
        let payload:Option<String>=sqlx::query_scalar("SELECT payload_json FROM agent_run_events WHERE user_id=? AND run_id=? AND event_idx=? AND event_type=? LIMIT 1 FOR UPDATE")
            .bind(user_id).bind(run_id).bind(selected.revision).bind(SETTINGS).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
        let Some(payload) = payload else {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(false);
        };
        let source: serde_json::Value =
            serde_json::from_str(&payload).map_err(|e| e.to_string())?;
        if permission_selection(&source).ok().as_ref() != Some(selected) {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(false);
        }
        let snapshot = Self::settings_snapshot_tx(&mut tx, user_id, run_id).await?;
        if let Some(applied) = snapshot.permission.applied.as_ref() {
            if applied.selection.revision > selected.revision
                || (applied.owner_generation == generation && applied.round_index > round)
            {
                tx.rollback().await.map_err(|e| e.to_string())?;
                connection.release();
                return Ok(false);
            }
            if applied.selection == *selected
                && applied.round_index == round
                && applied.owner_generation == generation
            {
                tx.rollback().await.map_err(|e| e.to_string())?;
                connection.release();
                return Ok(true);
            }
        }
        let event = application_event(selected, generation, round, run_id, session_id, snapshot);
        self.append_settings_event_tx(&mut tx, &run, &event, Some(generation), None)
            .await?;
        tx.commit()
            .await
            .map_err(|e| format!("permission application commit unconfirmed: {e}"))?;
        connection.release();
        Ok(true)
    }

    pub(super) async fn finish_model_request_inner(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        outcome: &RunModelOutcome,
        resolved: Option<&ResolvedModelSelection>,
    ) -> Result<Option<RunModelOutcome>, String> {
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|e| e.to_string())?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|e| e.to_string())?;
        let Some(run) = self
            .load_run_metadata_for_exact_session_tx(
                &mut tx,
                user_id,
                session_id,
                run_id,
                RunProjection::Opening,
            )
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        if run.depth != 0
            || run.run_generation != outcome.owner_generation
            || run.status != STATUS_RUNNING
            || run.owner_pod_id.as_deref() != Some(self.owner_pod_id.as_str())
            || lock_durable_lineage_cancellation_markers_tx(&mut tx, &run)
                .await?
                .any()
        {
            return Ok(None);
        }
        let live: Option<i32> = sqlx::query_scalar("SELECT 1 FROM agent_runs WHERE user_id=? AND run_id=? AND owner_pod_id=? AND run_generation=? AND owner_lease_expires_at>=NOW(6) FOR UPDATE")
            .bind(user_id).bind(run_id).bind(&self.owner_pod_id)
            .bind(i64::try_from(outcome.owner_generation).map_err(|e| e.to_string())?)
            .fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
        if live.is_none() {
            return Ok(None);
        }
        let source: Option<String> = sqlx::query_scalar("SELECT payload_json FROM agent_run_events WHERE user_id=? AND run_id=? AND event_idx=? AND event_type=? LIMIT 1 FOR UPDATE")
            .bind(user_id).bind(run_id).bind(outcome.selection.revision).bind(SETTINGS)
            .fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
        let Some(source) = source else {
            return Ok(None);
        };
        let source: serde_json::Value = serde_json::from_str(&source).map_err(|e| e.to_string())?;
        if model_selection(&source).ok().as_ref() != Some(&outcome.selection) {
            return Ok(None);
        }

        let key = model_outcome_key(outcome.selection.revision);
        let existing: Option<String> = sqlx::query_scalar("SELECT payload_json FROM agent_run_events WHERE user_id=? AND run_id=? AND idempotency_key=? LIMIT 1 FOR UPDATE")
            .bind(user_id).bind(run_id).bind(&key).fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
        if let Some(existing) = existing {
            let event: serde_json::Value =
                serde_json::from_str(&existing).map_err(|e| e.to_string())?;
            return match read_settings_event(&event)?.change {
                RunSettingsChange::ModelOutcome(previous)
                    if previous.selection == outcome.selection =>
                {
                    Ok(Some(previous))
                }
                _ => Err("model outcome identity conflict".into()),
            };
        }
        let snapshot = Self::settings_snapshot_tx(&mut tx, user_id, run_id).await?;
        let (snapshot, decided) = finish_model_decision(snapshot, outcome.clone(), resolved)?;
        let effective_model = if matches!(decided.result, RunModelOutcomeKind::Applied { .. }) {
            Some(
                &snapshot
                    .model
                    .applied
                    .as_ref()
                    .expect("applied outcome has admitted identity")
                    .resolved,
            )
        } else {
            None
        };
        let event = settings_event(
            RunSettingsChange::ModelOutcome(decided.clone()),
            snapshot.clone(),
            key,
            run_id,
            session_id,
        );
        self.append_settings_event_tx(
            &mut tx,
            &run,
            &event,
            Some(outcome.owner_generation),
            effective_model,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| format!("model application commit unconfirmed: {e}"))?;
        connection.release();
        Ok(Some(decided))
    }
}
