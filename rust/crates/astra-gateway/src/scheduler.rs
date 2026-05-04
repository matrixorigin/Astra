//! Cron scheduler — polls gw_cron_jobs and executes due tasks.

use crate::cli_bridge::{self, CliProfile};
use crate::config::GatewayConfig;
use crate::runner::{OutboundMessage, OutboxDelivery};
use crate::store::{self, GatewayStore};
use crate::trace_model::{
    ConversationKey, GatewayRequest, MysqlTraceRepository, RunStatus, TraceWriter,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(test)]
const CLI_TIMEOUT: Duration = Duration::from_secs(300);

pub struct CronScheduler {
    store: Arc<dyn GatewayStore>,
    config: GatewayConfig,
    trace_repo: Arc<MysqlTraceRepository>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
}

impl CronScheduler {
    pub fn new(
        store: Arc<dyn GatewayStore>,
        config: GatewayConfig,
        trace_repo: Arc<MysqlTraceRepository>,
        outbound_tx: mpsc::Sender<OutboundMessage>,
    ) -> Self {
        Self {
            store,
            config,
            trace_repo,
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
        let jobs = match self.store.get_due_jobs().await {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(error = %e, "cron: query failed");
                return;
            }
        };

        for job in jobs {
            let job_id = &job.job_id;
            let platform = &job.platform;
            let chat_id = &job.chat_id;
            let message = &job.message;
            let cron_expr = &job.cron_expr;
            tracing::info!(job_id = %job_id, expr = %cron_expr, "cron: executing");

            let user_id = self.resolve_job_user_id(job_id).await.unwrap_or_default();

            if cron_expr == "once" {
                let text = format!("⏰ 提醒: {message}");
                let outbound = self
                    .enqueue_scheduler_outbox(
                        job_id, platform, chat_id, &user_id, "reminder", &text, None,
                    )
                    .await
                    .unwrap_or_else(|_| {
                        OutboundMessage::plain(platform.clone(), chat_id.clone(), text)
                    });
                if let Err(e) = self.outbound_tx.send(outbound).await {
                    tracing::warn!(job_id = %job_id, error = %e, "failed to send one-shot reminder");
                }
                if let Err(e) = self.store.delete_cron_job(job_id).await {
                    tracing::error!(job_id = %job_id, error = %e, "failed to delete one-shot cron job — will re-fire next tick");
                }
                continue;
            }

            let cli_profile = self.resolve_cli_profile(platform, &user_id).await;
            let cli_name = cli_profile.name().to_string();
            let workspace = self.resolve_workspace(platform, &user_id).await;
            let session_id = self
                .store
                .get_current_session(platform, chat_id, &cli_name)
                .await
                .ok()
                .flatten();
            let trace = self
                .begin_scheduler_trace(job_id, platform, chat_id, &cli_name, message)
                .await;
            let run_id = if let Some(writer) = trace.as_ref() {
                writer.start_run(&cli_name, session_id.clone()).await.ok()
            } else {
                None
            };

            let cli_future = cli_bridge::run_cli_with_context_and_timeout(
                &cli_profile,
                message,
                session_id.as_deref(),
                workspace.as_deref(),
                None,
                None,
                Some(Duration::from_secs(self.config.cli_timeout_secs.max(1))),
            );

            let response = match cli_future.await {
                Ok(r) => {
                    if let Some(ref sid) = r.session_id
                        && let Err(e) = self
                            .store
                            .set_current_session(platform, chat_id, "", sid, &cli_name)
                            .await
                    {
                        tracing::warn!(error = %e, "scheduler: failed to save session");
                    }
                    if let Some(writer) = trace.as_ref()
                        && let Some(ref run_id) = run_id
                    {
                        let status = if r.exit_code == 0 {
                            RunStatus::Succeeded
                        } else {
                            RunStatus::Failed
                        };
                        if let Err(e) = writer
                            .finish_run(run_id, status, Some(r.exit_code), Some(&r.stderr))
                            .await
                        {
                            tracing::warn!(error = %e, "scheduler: failed to finish run trace");
                        }
                    }
                    r.text.unwrap_or(r.stdout)
                }
                Err(e) => {
                    if let Some(writer) = trace.as_ref()
                        && let Some(ref run_id) = run_id
                        && let Err(te) = writer
                            .finish_run(run_id, RunStatus::Failed, None, Some(&e))
                            .await
                    {
                        tracing::warn!(error = %te, "scheduler: failed to finish run trace");
                    }
                    format!("⚠️ 执行失败: {e}")
                }
            };

            let prefix = format!("⏰ **定时任务 `{}`**\n\n", &job_id[..8.min(job_id.len())]);
            let body = format!("{prefix}{response}");
            if let Some(writer) = trace.as_ref() {
                match writer.enqueue_outbox(platform, chat_id, None, &body).await {
                    Ok(outbox_id) => {
                        if let Err(e) = self
                            .outbound_tx
                            .send(OutboundMessage::with_outbox(
                                platform.clone(),
                                chat_id.clone(),
                                body,
                                None,
                                OutboxDelivery {
                                    outbox_id,
                                    trace_id: writer.trace_id().clone(),
                                    request_id: writer.request_id().clone(),
                                },
                            ))
                            .await
                        {
                            tracing::warn!(error = %e, "scheduler: outbound send failed");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "scheduler outbox enqueue failed"),
                }
            } else if let Err(e) = self
                .outbound_tx
                .send(OutboundMessage::plain(
                    platform.clone(),
                    chat_id.clone(),
                    body,
                ))
                .await
            {
                tracing::warn!(error = %e, "scheduler: outbound send failed");
            }

            if let Err(e) = self.store.mark_job_run(job_id, cron_expr).await {
                tracing::warn!(job_id = %job_id, error = %e, "scheduler: failed to mark job run");
            }
        }
    }

    async fn resolve_cli_profile(&self, platform: &str, user_id: &str) -> CliProfile {
        let mut profile = if let Ok(Some(name)) = self
            .store
            .get_user_preference(platform, user_id, "cli_profile")
            .await
            && let Some(profile) = self.config.cli_profiles.get(&name)
        {
            profile.clone()
        } else {
            self.config.cli.clone()
        };
        let model_key = store::model_preference_key(profile.name());
        if let Ok(Some(model_name)) = self
            .store
            .get_user_preference(platform, user_id, &model_key)
            .await
        {
            match &mut profile {
                CliProfile::Astra { model, .. } | CliProfile::Claude { model, .. } => {
                    *model = Some(model_name)
                }
                _ => {}
            }
        }
        profile
    }

    async fn resolve_workspace(&self, platform: &str, user_id: &str) -> Option<std::path::PathBuf> {
        let ws = self
            .store
            .get_user_preference(platform, user_id, "workspace")
            .await
            .ok()
            .flatten()?;
        let path = std::path::PathBuf::from(ws);
        if path.is_dir() { Some(path) } else { None }
    }

    /// Resolve the user_id who created the cron job.
    async fn resolve_job_user_id(&self, job_id: &str) -> Option<String> {
        self.store.get_cron_job_user_id(job_id).await.ok().flatten()
    }

    async fn begin_scheduler_trace(
        &self,
        job_id: &str,
        platform: &str,
        chat_id: &str,
        cli_name: &str,
        message: &str,
    ) -> Option<TraceWriter<'_>> {
        let request = GatewayRequest::new(
            ConversationKey::new(platform, chat_id, cli_name),
            format!("cron-{job_id}"),
            "",
            message,
        );
        match TraceWriter::begin(self.trace_repo.as_ref(), request).await {
            Ok(writer) => Some(writer),
            Err(e) => {
                tracing::warn!(error = %e, "scheduler trace begin failed");
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn enqueue_scheduler_outbox(
        &self,
        job_id: &str,
        platform: &str,
        chat_id: &str,
        _user_id: &str,
        cli_name: &str,
        body: &str,
        reply_token: Option<String>,
    ) -> Result<OutboundMessage, String> {
        let writer = self
            .begin_scheduler_trace(job_id, platform, chat_id, cli_name, body)
            .await
            .ok_or_else(|| "scheduler trace unavailable".to_string())?;
        let outbox_id = writer
            .enqueue_outbox(platform, chat_id, reply_token.clone(), body)
            .await?;
        Ok(OutboundMessage::with_outbox(
            platform.to_string(),
            chat_id.to_string(),
            body.to_string(),
            reply_token,
            OutboxDelivery {
                outbox_id,
                trace_id: writer.trace_id().clone(),
                request_id: writer.request_id().clone(),
            },
        ))
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
