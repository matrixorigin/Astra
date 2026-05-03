//! MySQL-backed DurableTaskStore implementation for the gateway.

use astra_core::durable_task_store::*;
use sqlx::MySqlPool;

pub struct MysqlDurableTaskStore {
    pool: MySqlPool,
}

impl MysqlDurableTaskStore {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }
}

type TaskRow = (
    String, String, Option<String>, String, String,
    u8, Option<String>, Option<String>, Option<String>,
    String, String,
);

const SELECT_COLS: &str =
    "SELECT task_id, name, description, owner_id, status, \
     progress_pct, step_description, checkpoint_json, error_message, \
     CAST(created_at AS CHAR), CAST(updated_at AS CHAR)";

#[async_trait::async_trait]
impl DurableTaskStore for MysqlDurableTaskStore {
    async fn create(&self, spec: &TaskSpec) -> Result<TaskId, String> {
        if spec.name.trim().is_empty() {
            return Err("task name cannot be empty".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let checkpoint_json = spec.initial_state.as_ref().map(|v| v.to_string());

        sqlx::query(
            "INSERT INTO gw_durable_tasks (task_id, name, description, owner_id, status, checkpoint_json)
             VALUES (?, ?, ?, ?, 'created', ?)",
        )
        .bind(&id)
        .bind(spec.name.trim())
        .bind(&spec.description)
        .bind(&spec.owner_id)
        .bind(&checkpoint_json)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create task failed: {e}"))?;

        Ok(TaskId(id))
    }

    async fn get(&self, id: &TaskId) -> Result<Option<DurableTask>, String> {
        let row: Option<TaskRow> = sqlx::query_as(
            &format!("{SELECT_COLS} FROM gw_durable_tasks WHERE task_id = ?"),
        )
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("get task failed: {e}"))?;

        Ok(row.map(row_to_task))
    }

    async fn list(&self, filter: TaskFilter) -> Result<Vec<DurableTask>, String> {
        let limit = filter.limit.unwrap_or(50);
        let rows: Vec<TaskRow> = match (&filter.owner_id, &filter.status) {
            (Some(owner), Some(status)) => {
                sqlx::query_as(
                    &format!("{SELECT_COLS} FROM gw_durable_tasks WHERE owner_id = ? AND status = ? ORDER BY updated_at DESC LIMIT ?"),
                )
                .bind(owner).bind(status.as_str()).bind(limit)
                .fetch_all(&self.pool).await
            }
            (Some(owner), None) => {
                sqlx::query_as(
                    &format!("{SELECT_COLS} FROM gw_durable_tasks WHERE owner_id = ? ORDER BY updated_at DESC LIMIT ?"),
                )
                .bind(owner).bind(limit)
                .fetch_all(&self.pool).await
            }
            (None, Some(status)) => {
                sqlx::query_as(
                    &format!("{SELECT_COLS} FROM gw_durable_tasks WHERE status = ? ORDER BY updated_at DESC LIMIT ?"),
                )
                .bind(status.as_str()).bind(limit)
                .fetch_all(&self.pool).await
            }
            (None, None) => {
                sqlx::query_as(
                    &format!("{SELECT_COLS} FROM gw_durable_tasks ORDER BY updated_at DESC LIMIT ?"),
                )
                .bind(limit)
                .fetch_all(&self.pool).await
            }
        }
        .map_err(|e| format!("list tasks failed: {e}"))?;

        Ok(rows.into_iter().map(row_to_task).collect())
    }

    async fn checkpoint(
        &self,
        id: &TaskId,
        state: &serde_json::Value,
        progress_pct: Option<u8>,
        step_description: Option<&str>,
    ) -> Result<(), String> {
        validate_checkpoint(state)?;
        let json_str = serde_json::to_string(state).map_err(|e| format!("serialize: {e}"))?;
        let progress = progress_pct.unwrap_or(0).min(100);

        let result = sqlx::query(
            "UPDATE gw_durable_tasks
             SET checkpoint_json = ?, progress_pct = ?, step_description = ?,
                 status = 'running', updated_at = NOW(6)
             WHERE task_id = ? AND status IN ('created', 'running', 'suspended')",
        )
        .bind(&json_str)
        .bind(progress)
        .bind(step_description)
        .bind(&id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("checkpoint failed: {e}"))?;

        if result.rows_affected() == 0 {
            return Err(format!("task {} not found or already terminal", id.0));
        }
        Ok(())
    }

    async fn update_status(
        &self,
        id: &TaskId,
        status: DurableTaskStatus,
        error_message: Option<&str>,
    ) -> Result<(), String> {
        let result = sqlx::query(
            "UPDATE gw_durable_tasks SET status = ?, error_message = ?, updated_at = NOW(6)
             WHERE task_id = ?",
        )
        .bind(status.as_str())
        .bind(error_message)
        .bind(&id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("update status failed: {e}"))?;

        if result.rows_affected() == 0 {
            return Err(format!("task {} not found", id.0));
        }
        Ok(())
    }

    async fn resume(&self, id: &TaskId) -> Result<Option<serde_json::Value>, String> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT checkpoint_json FROM gw_durable_tasks WHERE task_id = ?",
        )
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("resume failed: {e}"))?;

        match row {
            None => Err(format!("task {} not found", id.0)),
            Some((None,)) => Ok(None),
            Some((Some(json_str),)) => {
                let value = serde_json::from_str(&json_str)
                    .map_err(|e| format!("checkpoint parse error: {e}"))?;
                Ok(Some(value))
            }
        }
    }

    async fn delete(&self, id: &TaskId) -> Result<bool, String> {
        let result = sqlx::query("DELETE FROM gw_durable_tasks WHERE task_id = ?")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("delete failed: {e}"))?;
        Ok(result.rows_affected() > 0)
    }
}

fn row_to_task(r: TaskRow) -> DurableTask {
    DurableTask {
        id: TaskId(r.0),
        name: r.1,
        description: r.2,
        owner_id: r.3,
        status: DurableTaskStatus::parse(&r.4).unwrap_or(DurableTaskStatus::Created),
        progress_pct: r.5,
        step_description: r.6,
        checkpoint: r.7.as_deref().and_then(|s| serde_json::from_str(s).ok()),
        error_message: r.8,
        created_at: r.9,
        updated_at: r.10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_to_task_basic() {
        let row: TaskRow = (
            "id-1".into(), "test".into(), Some("desc".into()), "owner".into(), "running".into(),
            50u8, Some("step 2".into()), Some(r#"{"k":"v"}"#.into()), None,
            "2026-01-01".into(), "2026-01-02".into(),
        );
        let task = row_to_task(row);
        assert_eq!(task.id, TaskId("id-1".into()));
        assert_eq!(task.status, DurableTaskStatus::Running);
        assert_eq!(task.progress_pct, 50);
        assert_eq!(task.checkpoint.unwrap()["k"], "v");
    }

    #[test]
    fn row_to_task_null_checkpoint() {
        let row: TaskRow = (
            "id-2".into(), "t".into(), None, "o".into(), "created".into(),
            0u8, None, None, None,
            "2026-01-01".into(), "2026-01-01".into(),
        );
        let task = row_to_task(row);
        assert!(task.checkpoint.is_none());
    }

    #[test]
    fn row_to_task_unknown_status_defaults() {
        let row: TaskRow = (
            "id-3".into(), "t".into(), None, "o".into(), "garbage".into(),
            0u8, None, None, None,
            "2026-01-01".into(), "2026-01-01".into(),
        );
        let task = row_to_task(row);
        assert_eq!(task.status, DurableTaskStatus::Created);
    }
}
