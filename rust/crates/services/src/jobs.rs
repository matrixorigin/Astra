use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use astra_core::{ErrorResponse, error_response, internal_error};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct JobSubmitRequestData {
    pub job_type: String,
    pub inputs: serde_json::Value,
    pub gpu_required: bool,
    pub timeout_seconds: i32,
    pub conda_env: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JobRecord {
    pub job_id: String,
    pub status: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub progress: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JobWebhookData {
    pub job_id: String,
    pub status: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait JobService: Send + Sync {
    async fn submit_job(
        &self,
        user_id: String,
        request: JobSubmitRequestData,
    ) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_job(&self, job_id: String)
    -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn cancel_job(
        &self,
        job_id: String,
    ) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn job_webhook(
        &self,
        payload: JobWebhookData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)>;
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredJobService;

#[async_trait]
impl JobService for UnconfiguredJobService {
    async fn submit_job(
        &self,
        _: String,
        _: JobSubmitRequestData,
    ) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("job service not configured"))
    }
    async fn get_job(&self, _: String) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("job service not configured"))
    }
    async fn cancel_job(&self, _: String) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("job service not configured"))
    }
    async fn job_webhook(
        &self,
        _: JobWebhookData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("job service not configured"))
    }
}

// ── In-memory implementation ─────────────────────────────────────────────────

use std::collections::HashMap;
use std::sync::Mutex;

pub struct InMemoryJobService {
    jobs: Mutex<HashMap<String, JobRecord>>,
}

impl Default for InMemoryJobService {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryJobService {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl JobService for InMemoryJobService {
    async fn submit_job(
        &self,
        _user_id: String,
        _request: JobSubmitRequestData,
    ) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)> {
        let job_id = Uuid::new_v4().to_string();
        let record = JobRecord {
            job_id: job_id.clone(),
            status: "pending".into(),
            result: None,
            error: None,
            progress: 0.0,
        };
        self.jobs
            .lock()
            .expect("jobs mutex")
            .insert(job_id, record.clone());
        Ok(record)
    }

    async fn get_job(
        &self,
        job_id: String,
    ) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&job_id)
            .cloned()
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Job not found"))
    }

    async fn cancel_job(
        &self,
        job_id: String,
    ) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)> {
        let mut jobs = self.jobs.lock().expect("jobs mutex");
        let job = jobs
            .get(&job_id)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Job not found"))?;
        if matches!(job.status.as_str(), "completed" | "failed" | "cancelled") {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!("Job already {}", job.status),
            ));
        }
        let updated = JobRecord {
            status: "cancelled".into(),
            ..job.clone()
        };
        jobs.insert(job_id, updated.clone());
        Ok(updated)
    }

    async fn job_webhook(
        &self,
        payload: JobWebhookData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        let mut jobs = self.jobs.lock().expect("jobs mutex");
        if let Some(job) = jobs.get_mut(&payload.job_id) {
            job.status = payload.status;
            job.result = payload.result;
            job.error = payload.error;
        }
        Ok(serde_json::json!({"resumed": true, "job_id": payload.job_id}))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct JobSubmitRequest {
    pub job_type: String,
    #[serde(default)]
    pub inputs: serde_json::Value,
    #[serde(default)]
    pub gpu_required: bool,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: i32,
    pub conda_env: Option<String>,
}

pub fn default_timeout() -> i32 {
    3600
}

#[derive(Serialize, Deserialize)]
pub struct JobResponse {
    pub job_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub progress: f64,
}

impl From<JobRecord> for JobResponse {
    fn from(r: JobRecord) -> Self {
        Self {
            job_id: r.job_id,
            status: r.status,
            result: r.result,
            error: r.error,
            progress: r.progress,
        }
    }
}

#[derive(Deserialize)]
pub struct JobWebhookRequest {
    pub job_id: String,
    pub status: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to unwrap Result with non-Debug error type.
    fn unwrap_ok<T>(r: Result<T, (StatusCode, Json<ErrorResponse>)>) -> T {
        match r {
            Ok(v) => v,
            Err((status, _)) => panic!("expected Ok, got error with status {}", status),
        }
    }

    fn assert_err_status(
        r: Result<impl std::fmt::Debug, (StatusCode, Json<ErrorResponse>)>,
        expected: StatusCode,
    ) {
        match r {
            Ok(v) => panic!("expected error {}, got Ok({:?})", expected, v),
            Err((status, _)) => assert_eq!(status, expected),
        }
    }

    // ── InMemoryJobService basic flow ──

    #[tokio::test]
    async fn submit_and_get_job() {
        let svc = InMemoryJobService::new();
        let req = JobSubmitRequestData {
            job_type: "train".into(),
            inputs: serde_json::json!({"epochs": 10}),
            gpu_required: true,
            timeout_seconds: 1800,
            conda_env: Some("ml".into()),
        };
        let job = unwrap_ok(svc.submit_job("u1".into(), req).await);
        assert_eq!(job.status, "pending");
        assert_eq!(job.progress, 0.0);

        let fetched = unwrap_ok(svc.get_job(job.job_id.clone()).await);
        assert_eq!(fetched.job_id, job.job_id);
    }

    #[tokio::test]
    async fn get_nonexistent_job_returns_not_found() {
        let svc = InMemoryJobService::new();
        assert_err_status(
            svc.get_job("nonexistent".into()).await,
            StatusCode::NOT_FOUND,
        );
    }

    #[tokio::test]
    async fn cancel_pending_job() {
        let svc = InMemoryJobService::new();
        let req = JobSubmitRequestData {
            job_type: "train".into(),
            inputs: serde_json::json!({}),
            gpu_required: false,
            timeout_seconds: 3600,
            conda_env: None,
        };
        let job = unwrap_ok(svc.submit_job("u1".into(), req).await);
        let cancelled = unwrap_ok(svc.cancel_job(job.job_id).await);
        assert_eq!(cancelled.status, "cancelled");
    }

    #[tokio::test]
    async fn cancel_nonexistent_job_returns_not_found() {
        let svc = InMemoryJobService::new();
        assert_err_status(
            svc.cancel_job("nonexistent".into()).await,
            StatusCode::NOT_FOUND,
        );
    }

    #[tokio::test]
    async fn cancel_already_terminal_job_returns_conflict() {
        let svc = InMemoryJobService::new();
        let req = JobSubmitRequestData {
            job_type: "train".into(),
            inputs: serde_json::json!({}),
            gpu_required: false,
            timeout_seconds: 3600,
            conda_env: None,
        };
        let job = unwrap_ok(svc.submit_job("u1".into(), req).await);
        unwrap_ok(svc.cancel_job(job.job_id.clone()).await);
        assert_err_status(svc.cancel_job(job.job_id).await, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn webhook_updates_job_state() {
        let svc = InMemoryJobService::new();
        let req = JobSubmitRequestData {
            job_type: "train".into(),
            inputs: serde_json::json!({}),
            gpu_required: false,
            timeout_seconds: 3600,
            conda_env: None,
        };
        let job = unwrap_ok(svc.submit_job("u1".into(), req).await);

        let result = unwrap_ok(
            svc.job_webhook(JobWebhookData {
                job_id: job.job_id.clone(),
                status: "completed".into(),
                result: Some(serde_json::json!({"accuracy": 0.95})),
                error: None,
            })
            .await,
        );
        assert_eq!(result["resumed"], true);

        let updated = unwrap_ok(svc.get_job(job.job_id).await);
        assert_eq!(updated.status, "completed");
        assert!(updated.result.is_some());
    }

    #[tokio::test]
    async fn webhook_for_nonexistent_job_still_succeeds() {
        let svc = InMemoryJobService::new();
        let result = unwrap_ok(
            svc.job_webhook(JobWebhookData {
                job_id: "nonexistent".into(),
                status: "completed".into(),
                result: None,
                error: None,
            })
            .await,
        );
        assert_eq!(result["resumed"], true);
    }

    // ── UnconfiguredJobService ──

    #[tokio::test]
    async fn unconfigured_service_returns_errors() {
        let svc = UnconfiguredJobService;
        let req = JobSubmitRequestData {
            job_type: "train".into(),
            inputs: serde_json::json!({}),
            gpu_required: false,
            timeout_seconds: 3600,
            conda_env: None,
        };
        assert!(svc.submit_job("u1".into(), req).await.is_err());
        assert!(svc.get_job("j1".into()).await.is_err());
        assert!(svc.cancel_job("j1".into()).await.is_err());
        assert!(
            svc.job_webhook(JobWebhookData {
                job_id: "j1".into(),
                status: "done".into(),
                result: None,
                error: None,
            })
            .await
            .is_err()
        );
    }

    // ── HTTP types ──

    #[test]
    fn job_submit_request_defaults() {
        let json = r#"{"job_type":"train"}"#;
        let r: JobSubmitRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.job_type, "train");
        assert!(!r.gpu_required);
        assert_eq!(r.timeout_seconds, 3600);
        assert!(r.conda_env.is_none());
        assert_eq!(r.inputs, serde_json::Value::Null);
    }

    #[test]
    fn job_submit_request_full() {
        let json = r#"{"job_type":"train","inputs":{"lr":0.01},"gpu_required":true,"timeout_seconds":7200,"conda_env":"ml"}"#;
        let r: JobSubmitRequest = serde_json::from_str(json).unwrap();
        assert!(r.gpu_required);
        assert_eq!(r.timeout_seconds, 7200);
        assert_eq!(r.conda_env.as_deref(), Some("ml"));
    }

    #[test]
    fn job_response_skip_serializing_none() {
        let r = JobResponse {
            job_id: "j1".into(),
            status: "pending".into(),
            result: None,
            error: None,
            progress: 0.0,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("result"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn job_response_from_record() {
        let rec = JobRecord {
            job_id: "j1".into(),
            status: "completed".into(),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
            progress: 1.0,
        };
        let resp: JobResponse = rec.into();
        assert_eq!(resp.job_id, "j1");
        assert_eq!(resp.progress, 1.0);
        assert!(resp.result.is_some());
    }

    #[test]
    fn job_webhook_request_deserialize() {
        let json = r#"{"job_id":"j1","status":"failed","result":null,"error":"OOM"}"#;
        let r: JobWebhookRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.job_id, "j1");
        assert_eq!(r.error.as_deref(), Some("OOM"));
    }

    #[test]
    fn default_timeout_value() {
        assert_eq!(default_timeout(), 3600);
    }
}
