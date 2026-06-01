use super::*;
use sqlx::Row;
use std::sync::OnceLock;
use tokio::sync::broadcast;
use uuid::Uuid;

const SWEEP_INTERVAL_SECS: u64 = 300;

static DEVICE_LEASE_EVENTS: OnceLock<broadcast::Sender<serde_json::Value>> = OnceLock::new();

fn event_sender() -> &'static broadcast::Sender<serde_json::Value> {
    DEVICE_LEASE_EVENTS.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(1024);
        tx
    })
}

pub fn subscribe_device_lease_events() -> broadcast::Receiver<serde_json::Value> {
    event_sender().subscribe()
}

pub fn publish_device_lease_event(event: serde_json::Value) {
    let _ = event_sender().send(event);
}

pub async fn expire_due_device_leases_once(
    pool: SharedPool,
    limit: u32,
) -> Result<usize, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT lease_id, user_id, session_id, device_id, device_fingerprint
         FROM session_device_leases
         WHERE status = 'active' AND expires_at <= NOW(6)
         ORDER BY expires_at ASC
         LIMIT ?",
    )
    .bind(i64::from(limit.max(1)))
    .fetch_all(pool.get())
    .await?;
    let mut expired = 0usize;

    for row in rows {
        let lease_id = row.try_get::<String, _>("lease_id").unwrap_or_default();
        let user_id = row.try_get::<String, _>("user_id").unwrap_or_default();
        let session_id = row.try_get::<String, _>("session_id").unwrap_or_default();
        let device_id = row.try_get::<String, _>("device_id").unwrap_or_default();
        let device_fingerprint = row
            .try_get::<String, _>("device_fingerprint")
            .unwrap_or_default();
        let result = sqlx::query(
            "UPDATE session_device_leases
             SET status = 'expired', revoked_at = NOW(6), updated_at = NOW(6)
             WHERE lease_id = ? AND status = 'active' AND expires_at <= NOW(6)",
        )
        .bind(&lease_id)
        .execute(pool.get())
        .await?;
        if result.rows_affected() != 1 {
            continue;
        }

        let ended_at_server = chrono::Utc::now().naive_utc();
        sqlx::query(
            "INSERT INTO session_device_lease_events
             (lease_event_id, lease_id, user_id, session_id, device_id, device_fingerprint,
              event_type, reason, ended_at_server, created_at)
             VALUES (?, ?, ?, ?, ?, ?, 'auto_expire', 'auto_expire', ?, NOW(6))",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&lease_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(&device_id)
        .bind(&device_fingerprint)
        .bind(ended_at_server)
        .execute(pool.get())
        .await?;

        publish_device_lease_event(serde_json::json!({
            "type": "device_lease_expired",
            "lease_id": lease_id,
            "session_id": session_id,
            "device_id": device_id,
            "device_fingerprint": device_fingerprint,
            "reason": "auto_expire",
            "ended_at_server": ended_at_server.format("%Y-%m-%dT%H:%M:%S").to_string(),
        }));
        expired += 1;
    }

    Ok(expired)
}

pub(crate) fn spawn_device_lease_expiry_sweeper(
    pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match lease.check_leader().await {
                crate::server::sweeper_lease::LeaderStatus::Leader => {}
                crate::server::sweeper_lease::LeaderStatus::NotLeader => continue,
                crate::server::sweeper_lease::LeaderStatus::Unavailable(e) => {
                    tracing::warn!(
                        target: "astra_runtime::device_lease_sweeper",
                        error = %e,
                        "sweeper lease check unavailable, skipping sweep"
                    );
                    continue;
                }
            }
            if let Err(error) = expire_due_device_leases_once(pool.clone(), 500).await {
                tracing::warn!(
                    target: "astra_runtime::device_lease_sweeper",
                    error = %error,
                    "device lease expiry sweeper failed"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn sweeper_interval_is_five_minutes() {
        assert_eq!(super::SWEEP_INTERVAL_SECS, 300);
    }
}
