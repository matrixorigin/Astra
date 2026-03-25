use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use mo_agent_core::{ErrorResponse, error_response, internal_error};

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
        self.jobs.lock().unwrap().insert(job_id, record.clone());
        Ok(record)
    }

    async fn get_job(
        &self,
        job_id: String,
    ) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)> {
        self.jobs
            .lock()
            .unwrap()
            .get(&job_id)
            .cloned()
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Job not found"))
    }

    async fn cancel_job(
        &self,
        job_id: String,
    ) -> Result<JobRecord, (StatusCode, Json<ErrorResponse>)> {
        let mut jobs = self.jobs.lock().unwrap();
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
        let mut jobs = self.jobs.lock().unwrap();
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
