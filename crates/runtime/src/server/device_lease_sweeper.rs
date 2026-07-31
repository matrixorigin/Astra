use super::*;
use crate::db_row::RowExt;
use std::{fmt::Display, sync::OnceLock};
use tokio::sync::broadcast;
use uuid::Uuid;

const SWEEP_INTERVAL_SECS: u64 = 300;

static DEVICE_LEASE_EVENTS: OnceLock<broadcast::Sender<DeviceLeaseEvent>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceLeaseEvent {
    pub owner_user_id: String,
    pub session_id: String,
    pub payload: serde_json::Value,
}

impl DeviceLeaseEvent {
    pub fn belongs_to(&self, owner_user_id: &str, session_id: &str) -> bool {
        self.owner_user_id == owner_user_id && self.session_id == session_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpiringDeviceLeaseRow {
    lease_id: String,
    user_id: String,
    session_id: String,
    device_id: String,
    device_fingerprint: String,
}

fn device_lease_decode_error(column: &'static str, error: impl Display) -> String {
    format!("expiring device lease row decode `{column}`: {error}")
}

fn expiring_device_lease_string(row: &impl RowExt, column: &'static str) -> Result<String, String> {
    let value = row
        .string_column(column)
        .map_err(|error| device_lease_decode_error(column, error))?;
    if value.trim().is_empty() {
        return Err(device_lease_decode_error(
            column,
            "expected non-empty string",
        ));
    }
    Ok(value)
}

fn decode_expiring_device_lease_row(row: &impl RowExt) -> Result<ExpiringDeviceLeaseRow, String> {
    Ok(ExpiringDeviceLeaseRow {
        lease_id: expiring_device_lease_string(row, "lease_id")?,
        user_id: expiring_device_lease_string(row, "user_id")?,
        session_id: expiring_device_lease_string(row, "session_id")?,
        device_id: expiring_device_lease_string(row, "device_id")?,
        device_fingerprint: expiring_device_lease_string(row, "device_fingerprint")?,
    })
}

fn event_sender() -> &'static broadcast::Sender<DeviceLeaseEvent> {
    DEVICE_LEASE_EVENTS.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(1024);
        tx
    })
}

pub fn subscribe_device_lease_events() -> broadcast::Receiver<DeviceLeaseEvent> {
    event_sender().subscribe()
}

pub fn publish_device_lease_event(
    owner_user_id: impl Into<String>,
    session_id: impl Into<String>,
    payload: serde_json::Value,
) {
    let sender = event_sender();
    if sender.receiver_count() == 0 {
        return;
    }
    let event = DeviceLeaseEvent {
        owner_user_id: owner_user_id.into(),
        session_id: session_id.into(),
        payload,
    };
    if let Err(error) = sender.send(event) {
        tracing::debug!(
            target: "astra_runtime::device_lease_sweeper",
            %error,
            "device lease event had no live subscriber"
        );
    }
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
        let lease = decode_expiring_device_lease_row(&row).map_err(sqlx::Error::Protocol)?;
        let result = sqlx::query(
            "UPDATE session_device_leases
             SET status = 'expired', revoked_at = NOW(6), updated_at = NOW(6)
             WHERE user_id = ? AND lease_id = ? AND session_id = ?
               AND device_id = ? AND device_fingerprint = ?
               AND status = 'active' AND expires_at <= NOW(6)",
        )
        .bind(&lease.user_id)
        .bind(&lease.lease_id)
        .bind(&lease.session_id)
        .bind(&lease.device_id)
        .bind(&lease.device_fingerprint)
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
        .bind(&lease.lease_id)
        .bind(&lease.user_id)
        .bind(&lease.session_id)
        .bind(&lease.device_id)
        .bind(&lease.device_fingerprint)
        .bind(ended_at_server)
        .execute(pool.get())
        .await?;

        publish_device_lease_event(
            &lease.user_id,
            &lease.session_id,
            serde_json::json!({
                "type": "device_lease_expired",
                "lease_id": lease.lease_id,
                "session_id": lease.session_id,
                "device_id": lease.device_id,
                "device_fingerprint": lease.device_fingerprint,
                "reason": "auto_expire",
                "ended_at_server": ended_at_server.format("%Y-%m-%dT%H:%M:%S").to_string(),
            }),
        );
        expired += 1;
    }

    Ok(expired)
}

pub(crate) fn spawn_device_lease_expiry_sweeper(
    pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    'tick: {
                        match lease.check_leader().await {
                            crate::server::sweeper_lease::LeaderStatus::Leader => {}
                            crate::server::sweeper_lease::LeaderStatus::NotLeader => break 'tick,
                            crate::server::sweeper_lease::LeaderStatus::Unavailable(e) => {
                                tracing::warn!(
                                    target: "astra_runtime::device_lease_sweeper",
                                    error = %e,
                                    "sweeper lease check unavailable, skipping sweep"
                                );
                                break 'tick;
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
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeExpiringDeviceLeaseRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
    }

    impl FakeExpiringDeviceLeaseRow {
        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                empty_column: None,
            }
        }

        fn empty_on(column: &'static str) -> Self {
            Self {
                failed_column: None,
                empty_column: Some(column),
            }
        }
    }

    impl RowExt for FakeExpiringDeviceLeaseRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            if self.empty_column == Some(column) {
                return Ok(String::new());
            }
            let value = match column {
                "lease_id" => "lease-1",
                "user_id" => "user-1",
                "session_id" => "session-1",
                "device_id" => "device-1",
                "device_fingerprint" => "fingerprint-1",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            };
            Ok(value.to_string())
        }
    }

    #[test]
    fn expiring_device_lease_row_decode_preserves_database_values() {
        let row = decode_expiring_device_lease_row(&FakeExpiringDeviceLeaseRow::default()).unwrap();

        assert_eq!(
            row,
            ExpiringDeviceLeaseRow {
                lease_id: "lease-1".to_string(),
                user_id: "user-1".to_string(),
                session_id: "session-1".to_string(),
                device_id: "device-1".to_string(),
                device_fingerprint: "fingerprint-1".to_string(),
            }
        );
    }

    #[test]
    fn expiring_device_lease_row_decode_fails_loudly_on_any_selected_column_error() {
        for column in [
            "lease_id",
            "user_id",
            "session_id",
            "device_id",
            "device_fingerprint",
        ] {
            let error =
                decode_expiring_device_lease_row(&FakeExpiringDeviceLeaseRow::fail_on(column))
                    .unwrap_err();
            assert!(
                error.contains("expiring device lease row decode") && error.contains(column),
                "decode error should identify selected column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn expiring_device_lease_row_decode_rejects_empty_identity_columns() {
        for column in [
            "lease_id",
            "user_id",
            "session_id",
            "device_id",
            "device_fingerprint",
        ] {
            let error =
                decode_expiring_device_lease_row(&FakeExpiringDeviceLeaseRow::empty_on(column))
                    .unwrap_err();
            assert!(
                error.contains(column) && error.contains("non-empty string"),
                "empty identity column should fail loudly for `{column}`: {error}"
            );
        }
    }

    #[test]
    fn sweeper_interval_is_five_minutes() {
        assert_eq!(super::SWEEP_INTERVAL_SECS, 300);
    }

    #[test]
    fn live_event_scope_requires_both_owner_and_session() {
        let event = DeviceLeaseEvent {
            owner_user_id: "owner-a".to_string(),
            session_id: "shared-session".to_string(),
            payload: serde_json::json!({"type": "device_revoked"}),
        };

        assert!(event.belongs_to("owner-a", "shared-session"));
        assert!(!event.belongs_to("owner-b", "shared-session"));
        assert!(!event.belongs_to("owner-a", "other-session"));
    }
}
