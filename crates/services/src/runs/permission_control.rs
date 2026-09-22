//! Durable permission requests share the run journal and owner boundary.
//! A request affects the next policy capture, never invalidates an active round.
use super::*;

const REQUESTED: &str = "permission_mode_requested";
const APPLIED: &str = "permission_mode_applied";

fn validate_request(request: &RunPermissionModeRequest) -> Result<(), String> {
    if request.expected_session_id.trim().is_empty()
        || request.request_id.trim().is_empty()
        || request.request_id.len() > 128
        || request.request_id.trim() != request.request_id
    {
        return Err("permission request requires a session and a nonempty request_id of at most 128 bytes without surrounding whitespace".into());
    }
    Ok(())
}

fn request_event(request: &RunPermissionModeRequest) -> serde_json::Value {
    serde_json::json!({"event_type":REQUESTED,
        "idempotency_key":format!("permission_mode_requested:{}",request.request_id),
        "data":{"request_id":request.request_id,"mode":request.mode}})
}

fn selection(
    event: &serde_json::Value,
    revision: i64,
) -> Result<RunPermissionModeSelection, String> {
    let mut data = event
        .get("data")
        .cloned()
        .ok_or("missing permission event data")?;
    data.as_object_mut()
        .ok_or("permission event data is not an object")?
        .insert("revision".into(), revision.into());
    serde_json::from_value(data).map_err(|e| format!("invalid permission request event: {e}"))
}

fn snapshot_from_events(events: &[serde_json::Value]) -> Result<RunPermissionModeSnapshot, String> {
    let requested = events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, e)| extract_event_type(e) == REQUESTED)
        .map(|(i, e)| selection(e, i as i64))
        .transpose()?;
    let applied = events
        .iter()
        .rev()
        .find(|e| extract_event_type(e) == APPLIED)
        .map(|e| serde_json::from_value(e["data"].clone()).map_err(|e| e.to_string()))
        .transpose()?;
    Ok(RunPermissionModeSnapshot { requested, applied })
}

fn application_event(
    selection: &RunPermissionModeSelection,
    generation: u64,
    round: u32,
    run_id: &str,
    session_id: &str,
) -> serde_json::Value {
    let mut data = serde_json::to_value(RunPermissionModeApplied {
        selection: selection.clone(),
        owner_generation: generation,
        round_index: round,
    })
    .expect("permission receipt is serializable");
    data["run_id"] = run_id.into();
    data["session_id"] = session_id.into();
    serde_json::json!({"event_type":APPLIED,
        "idempotency_key":format!("permission_mode_applied:{}:{}:{}",generation,round,selection.revision),
        "data":data})
}

impl InMemoryRunStateStore {
    pub(super) async fn request_permission_mode_inner(
        &self,
        user_id: &str,
        run_id: &str,
        request: &RunPermissionModeRequest,
    ) -> Result<RunPermissionModeSelection, String> {
        validate_request(request)?;
        let fence = self.action_fence_for(user_id, run_id);
        let _guard = fence.lock_owned().await;
        let cancellations = self.cancellation_requests.read().await;
        let mut runs = self.runs.write().await;
        let run = runs
            .get_mut(run_id)
            .filter(|r| {
                r.user_id == user_id && r.session_id == request.expected_session_id && r.depth == 0
            })
            .ok_or("permission run not found")?;
        let event = request_event(request);
        if let Some((idx, existing)) = run
            .events
            .iter()
            .enumerate()
            .find(|(_, e)| e["idempotency_key"] == event["idempotency_key"])
        {
            if !run_events_have_same_immutable_payload(existing, &event) {
                return Err("permission request identity conflict".into());
            }
            return selection(existing, idx as i64);
        }
        if !matches!(run.status.as_str(), STATUS_RUNNING | STATUS_WAITING)
            || cancellations.contains(&(user_id.to_string(), run_id.to_string()))
        {
            return Err("permission run is inactive".into());
        }
        run.events.push(event.clone());
        run.last_event_idx = run.events.len() as i64 - 1;
        run.updated_at = chrono::Utc::now().to_rfc3339();
        selection(&event, run.last_event_idx)
    }

    pub(super) async fn permission_mode_snapshot_inner(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<RunPermissionModeSnapshot>, String> {
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
        if extract_event_type(event) != REQUESTED
            || selection(event, selected.revision)? != *selected
        {
            return Ok(false);
        }
        let snapshot = snapshot_from_events(&run.events)?;
        if snapshot.applied.as_ref().is_some_and(|a| {
            a.selection.revision > selected.revision
                || (a.owner_generation == generation && a.round_index > round)
        }) {
            return Ok(false);
        }
        let event = application_event(selected, generation, round, run_id, session_id);
        if let Some(existing) = run
            .events
            .iter()
            .find(|e| e["idempotency_key"] == event["idempotency_key"])
        {
            return Ok(run_events_have_same_immutable_payload(existing, &event));
        }
        run.events.push(event);
        run.last_event_idx = run.events.len() as i64 - 1;
        run.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(true)
    }
}

impl DatabaseRunStateStore {
    async fn permission_event_tx(
        tx: &mut sqlx::Transaction<'_, MySql>,
        user_id: &str,
        run_id: &str,
        kind: &str,
    ) -> Result<Option<(i64, serde_json::Value)>, String> {
        let row=sqlx::query("SELECT event_idx,payload_json FROM agent_run_events WHERE user_id=? AND run_id=? AND event_type=? ORDER BY event_idx DESC LIMIT 1")
            .bind(user_id).bind(run_id).bind(kind).fetch_optional(&mut **tx).await.map_err(|e|e.to_string())?;
        row.map(|r| {
            let idx: i64 = r.try_get("event_idx").map_err(|e| e.to_string())?;
            let payload: String = r.try_get("payload_json").map_err(|e| e.to_string())?;
            Ok((
                idx,
                serde_json::from_str(&payload).map_err(|e| e.to_string())?,
            ))
        })
        .transpose()
    }

    async fn permission_snapshot_tx(
        tx: &mut sqlx::Transaction<'_, MySql>,
        user_id: &str,
        run_id: &str,
    ) -> Result<RunPermissionModeSnapshot, String> {
        let requested = Self::permission_event_tx(tx, user_id, run_id, REQUESTED)
            .await?
            .map(|(i, e)| selection(&e, i))
            .transpose()?;
        let applied = Self::permission_event_tx(tx, user_id, run_id, APPLIED)
            .await?
            .map(|(_, e)| serde_json::from_value(e["data"].clone()).map_err(|e| e.to_string()))
            .transpose()?;
        Ok(RunPermissionModeSnapshot { requested, applied })
    }

    async fn append_permission_event_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, MySql>,
        run: &DurableRunRecord,
        event: &serde_json::Value,
        expected_generation: Option<u64>,
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
        update
            .push_bind(idx)
            .push(",updated_at=NOW(6) WHERE user_id=")
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
            return Err("permission event append lost run authority".into());
        }
        Self::insert_run_event_rows_tx(tx, &run.run_id, &[row], "permission_mode_append").await?;
        Ok(idx)
    }

    pub(super) async fn request_permission_mode_inner(
        &self,
        user_id: &str,
        run_id: &str,
        request: &RunPermissionModeRequest,
    ) -> Result<RunPermissionModeSelection, String> {
        validate_request(request)?;
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
                &request.expected_session_id,
                run_id,
            )
            .await
            .map_err(|e| e.to_string())?
            .filter(|r| r.depth == 0)
            .ok_or("permission run not found")?;
        let event = request_event(request);
        let row=sqlx::query("SELECT event_idx,payload_json FROM agent_run_events WHERE user_id=? AND run_id=? AND idempotency_key=? LIMIT 1")
            .bind(user_id).bind(run_id).bind(event["idempotency_key"].as_str().unwrap()).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
        if let Some(row) = row {
            let idx: i64 = row.try_get("event_idx").map_err(|e| e.to_string())?;
            let payload: String = row.try_get("payload_json").map_err(|e| e.to_string())?;
            let existing: serde_json::Value =
                serde_json::from_str(&payload).map_err(|e| e.to_string())?;
            if !run_events_have_same_immutable_payload(&existing, &event) {
                return Err("permission request identity conflict".into());
            }
            let selected = selection(&existing, idx)?;
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(selected);
        }
        if !matches!(run.status.as_str(), STATUS_RUNNING | STATUS_WAITING) {
            return Err("permission run is inactive".into());
        }
        if lock_durable_lineage_cancellation_markers_tx(&mut tx, &run)
            .await?
            .any()
        {
            return Err("permission run is cancelled".into());
        }
        let idx = self
            .append_permission_event_tx(&mut tx, &run, &event, None)
            .await?;
        tx.commit().await.map_err(|e| {
            format!("permission request commit unconfirmed; retry same request_id: {e}")
        })?;
        connection.release();
        selection(&event, idx)
    }

    pub(super) async fn permission_mode_snapshot_inner(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<RunPermissionModeSnapshot>, String> {
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|e| e.to_string())?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|e| e.to_string())?;
        // This hot round-boundary lookup must not fetch checkpoint_json or
        // hydrate the run history. Retain the canonical session/run ownership
        // fence, then read only the root marker and two indexed event rows.
        match crate::storage::admit_session_execution_write(&mut tx, session_id, user_id).await {
            Ok(()) => {}
            Err(sqlx::Error::RowNotFound) => {
                tx.rollback().await.map_err(|e| e.to_string())?;
                connection.release();
                return Ok(None);
            }
            Err(error) => return Err(error.to_string()),
        }
        let root: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM agent_runs WHERE user_id=? AND session_id=? AND run_id=? AND depth=0 LIMIT 1 FOR UPDATE",
        ).bind(user_id).bind(session_id).bind(run_id).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
        if root.is_none() {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(None);
        }
        let snapshot = Self::permission_snapshot_tx(&mut tx, user_id, run_id).await?;
        tx.rollback().await.map_err(|e| e.to_string())?;
        connection.release();
        Ok(Some(snapshot))
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
            .load_run_metadata_for_exact_session_tx(&mut tx, user_id, session_id, run_id)
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
        let payload:Option<String>=sqlx::query_scalar("SELECT payload_json FROM agent_run_events WHERE user_id=? AND run_id=? AND event_idx=? AND event_type=? LIMIT 1")
            .bind(user_id).bind(run_id).bind(selected.revision).bind(REQUESTED).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
        let Some(payload) = payload else {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(false);
        };
        let source: serde_json::Value =
            serde_json::from_str(&payload).map_err(|e| e.to_string())?;
        if selection(&source, selected.revision)? != *selected {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(false);
        }
        let snapshot = Self::permission_snapshot_tx(&mut tx, user_id, run_id).await?;
        if let Some(applied) = snapshot.applied {
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
        let event = application_event(selected, generation, round, run_id, session_id);
        self.append_permission_event_tx(&mut tx, &run, &event, Some(generation))
            .await?;
        tx.commit()
            .await
            .map_err(|e| format!("permission application commit unconfirmed: {e}"))?;
        connection.release();
        Ok(true)
    }
}
