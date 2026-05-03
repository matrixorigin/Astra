//! Cron scheduler — polls gw_cron_jobs and executes due tasks.

use crate::cli_bridge::{self, CliProfile};
use crate::storage;
use sqlx::MySqlPool;
use std::time::Duration;
use tokio::sync::mpsc;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const CLI_TIMEOUT: Duration = Duration::from_secs(300);

/// (platform, chat_id, text) — sent to runner for delivery.
pub type OutboundMessage = (String, String, String);

pub struct CronScheduler {
    pool: MySqlPool,
    cli_profile: CliProfile,
    outbound_tx: mpsc::Sender<OutboundMessage>,
}

impl CronScheduler {
    pub fn new(
        pool: MySqlPool,
        cli_profile: CliProfile,
        outbound_tx: mpsc::Sender<OutboundMessage>,
    ) -> Self {
        Self {
            pool,
            cli_profile,
            outbound_tx,
        }
    }

    pub fn spawn(
        self,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("cron scheduler started");
            let mut interval = tokio::time::interval(POLL_INTERVAL);

            loop {
                tokio::select! {
                    _ = interval.tick() => self.tick().await,
                    _ = shutdown.recv() => break,
                }
            }
            tracing::info!("cron scheduler stopped");
        })
    }

    async fn tick(&self) {
        let jobs = match storage::get_due_jobs(&self.pool).await {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(error = %e, "cron: query failed");
                return;
            }
        };

        for (job_id, platform, chat_id, message, cron_expr) in jobs {
            tracing::info!(job_id = %job_id, "cron: executing");

            let session_id = storage::get_current_session(&self.pool, &platform, &chat_id)
                .await
                .ok()
                .flatten();

            let cli_future = cli_bridge::run_cli(
                &self.cli_profile,
                &message,
                session_id.as_deref(),
                None,
                None,
            );

            let response = match tokio::time::timeout(CLI_TIMEOUT, cli_future).await {
                Ok(Ok(r)) => {
                    if let Some(ref sid) = r.session_id {
                        let _ = storage::set_current_session(
                            &self.pool, &platform, &chat_id, "", sid,
                        )
                        .await;
                    }
                    r.text.unwrap_or(r.stdout)
                }
                Ok(Err(e)) => format!("⚠️ 执行失败: {e}"),
                Err(_) => "⚠️ 执行超时 (5分钟)".into(),
            };

            let prefix = format!(
                "⏰ **定时任务 `{}`**\n\n",
                &job_id[..8.min(job_id.len())]
            );
            let _ = self
                .outbound_tx
                .send((platform.clone(), chat_id, format!("{prefix}{response}")))
                .await;

            let _ = storage::mark_job_run(&self.pool, &job_id, &cron_expr).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(POLL_INTERVAL.as_secs(), 60);
        assert_eq!(CLI_TIMEOUT.as_secs(), 300);
    }
}
