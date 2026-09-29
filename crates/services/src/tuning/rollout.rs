//! Authenticated callers manage one versioned deployment per owner. Runtime
//! decisions and reviewed outcomes stay in the canonical run-event ledger.
use crate::evaluation::router::content_sha256;
use crate::tuning::{RouterQualificationProtocol, RouterTuningRecord};
use astra_core::SharedPool;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutMode {
    Shadow,
    Canary,
    RolledBack,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutCohort {
    Shadow,
    Control,
    Treatment,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolloutReview {
    pub online_consent_reference: String,
    pub verifier_review_reference: String,
    pub safety_review_reference: String,
    pub expires_at: DateTime<Utc>,
    pub minimum_shadow_sessions: usize,
    pub maximum_routing_overhead_ms: u64,
}
impl RolloutReview {
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), String> {
        if [
            &self.online_consent_reference,
            &self.verifier_review_reference,
            &self.safety_review_reference,
        ]
        .iter()
        .any(|s| !opaque(s))
            || self.expires_at <= now
            || self.minimum_shadow_sessions == 0
            || self.maximum_routing_overhead_ms == 0
            || self.maximum_routing_overhead_ms > 1000
        {
            return Err("Invalid online consent, review, expiry or shadow budget".into());
        }
        Ok(())
    }
}
pub fn opaque(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterDeployment {
    pub deployment_id: String,
    pub owner_id: String,
    pub policy_revision: String,
    pub rubric_version: String,
    /// Serialized by the canonical turn-core trainer, pinned by tuning digest.
    pub candidate_json: String,
    pub tuning: RouterTuningRecord,
    pub protocol: RouterQualificationProtocol,
    pub review: RolloutReview,
    pub mode: RolloutMode,
    pub canary_basis_points: u16,
    pub assignment_salt: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub stop_reason: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RouterRolloutState {
    pub revision: u64,
    pub deployment: Option<RouterDeployment>,
    /// Retained across replacements: a new publication cannot resurrect lineage.
    pub revoked_source_ids: std::collections::BTreeSet<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterRolloutDecision {
    pub deployment_id: String,
    pub revision: u64,
    pub candidate_sha256: String,
    /// Immutable review contract for delayed outcomes after replacement.
    pub rubric_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_failure: Option<RouterRoutingFailure>,
    pub cohort: RolloutCohort,
    /// Probability of this cohort, not a model's quality or action propensity.
    pub cohort_probability_basis_points: u16,
    pub proposed_offering_id: String,
    pub abstained: bool,
    pub admission_rejected: bool,
    pub routing_overhead_us: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterRoutingFailure {
    OverheadBudgetExceeded,
}
impl RouterRolloutDecision {
    pub fn ensure_dispatchable(&self) -> Result<(), String> {
        if self.routing_failure.is_some() {
            return Err("Canary routing overhead exceeded its reviewed budget".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub enum RolloutChange {
    Publish(Box<RouterDeployment>),
    Canary {
        basis_points: u16,
    },
    Rollback {
        reason: String,
    },
    /// Stop this deployment under the row lock even if its revision advanced.
    /// A replacement deployment must never be stopped by an older report.
    CriticalViolation {
        deployment_id: String,
    },
    Revoke {
        source_ids: Vec<String>,
    },
}

pub fn transition(
    mut state: RouterRolloutState,
    owner: &str,
    change: RolloutChange,
    now: DateTime<Utc>,
) -> Result<RouterRolloutState, String> {
    if !opaque(owner) || owner.len() > 128 {
        return Err("Invalid rollout owner".into());
    }
    match change {
        RolloutChange::Publish(deployment) => {
            let d = *deployment;
            d.review.validate(now)?;
            if d.owner_id != owner
                || d.tuning.owner_id != owner
                || d.protocol.owner_id != owner
                || d.mode != RolloutMode::Shadow
                || d.canary_basis_points != 0
                || d.expires_at <= now
                || d.expires_at > d.tuning.expires_at
                || d.expires_at > d.review.expires_at
                || d.tuning.status != super::RouterQualificationStatus::ReadyForShadow
                || d.tuning.production_qualified
                || d.tuning.protocol_sha256 != content_sha256(&d.protocol)?
                || d.tuning.candidate_sha256 != candidate_json_sha256(&d.candidate_json)
                || d.tuning
                    .source_ids
                    .iter()
                    .any(|id| state.revoked_source_ids.contains(id))
                || state
                    .deployment
                    .as_ref()
                    .is_some_and(|d| d.mode != RolloutMode::RolledBack && d.expires_at > now)
            {
                return Err("Deployment is not a current authorized shadow candidate; rollback before replacing".into());
            }
            state.deployment = Some(d);
        }
        RolloutChange::Canary { basis_points } => {
            let d = state.deployment.as_mut().ok_or("No deployment")?;
            if d.mode != RolloutMode::Shadow
                || d.expires_at <= now
                || !(1..=1000).contains(&basis_points)
            {
                return Err(
                    "Canary requires current shadow deployment and 1..=1000 basis points".into(),
                );
            }
            d.mode = RolloutMode::Canary;
            d.canary_basis_points = basis_points;
        }
        RolloutChange::Rollback { reason } => {
            if !opaque(&reason) {
                return Err("Rollback reason must be an opaque code".into());
            }
            let d = state.deployment.as_mut().ok_or("No deployment")?;
            d.mode = RolloutMode::RolledBack;
            d.stop_reason = Some(reason);
        }
        RolloutChange::CriticalViolation { deployment_id } => {
            let d = state.deployment.as_mut().ok_or("No deployment")?;
            if d.deployment_id != deployment_id {
                return Err("Critical violation belongs to a different deployment".into());
            }
            d.mode = RolloutMode::RolledBack;
            d.stop_reason = Some("critical_violation".into());
        }
        RolloutChange::Revoke { source_ids } => {
            if source_ids.is_empty()
                || source_ids.len() > 100_000
                || source_ids.iter().any(|s| !opaque(s))
            {
                return Err("Invalid source revocations".into());
            }
            state.revoked_source_ids.extend(source_ids);
            if let Some(d) = &mut state.deployment
                && d.tuning
                    .source_ids
                    .iter()
                    .any(|id| state.revoked_source_ids.contains(id))
            {
                d.mode = RolloutMode::RolledBack;
                d.stop_reason = Some("source_revoked".into());
            }
        }
    }
    state.revision = state
        .revision
        .checked_add(1)
        .ok_or("Rollout revision exhausted")?;
    Ok(state)
}
#[async_trait]
pub trait RouterRolloutStore: Send + Sync {
    async fn load(&self, owner: &str) -> Result<RouterRolloutState, String>;
    async fn change(
        &self,
        owner: &str,
        expected: u64,
        actor: &str,
        change: RolloutChange,
    ) -> Result<RouterRolloutState, String>;
}
#[derive(Clone)]
pub struct DatabaseRouterRolloutStore(pub SharedPool);
#[async_trait]
impl RouterRolloutStore for DatabaseRouterRolloutStore {
    async fn load(&self, owner: &str) -> Result<RouterRolloutState, String> {
        let mut connection =
            crate::cancellation_safe_db::CancellationSafePoolConnection::acquire(self.0.get())
                .await
                .map_err(|e| e.to_string())?;
        let row = sqlx::query("SELECT state_json FROM model_router_deployments WHERE user_id = ?")
            .bind(owner)
            .fetch_optional(connection.connection_mut())
            .await
            .map_err(|e| e.to_string())?;
        connection.release();
        row.map(|r| {
            serde_json::from_str(&r.get::<String, _>("state_json")).map_err(|e| e.to_string())
        })
        .transpose()
        .map(|s| s.unwrap_or_default())
    }

    async fn change(
        &self,
        owner: &str,
        expected: u64,
        actor: &str,
        change: RolloutChange,
    ) -> Result<RouterRolloutState, String> {
        if !opaque(owner) || !opaque(actor) || owner.len() > 128 || actor.len() > 128 {
            return Err("Invalid rollout principal".into());
        }
        let mut connection =
            crate::cancellation_safe_db::CancellationSafePoolConnection::acquire(self.0.get())
                .await
                .map_err(|e| e.to_string())?;
        let mut tx = connection.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("INSERT IGNORE INTO model_router_deployments (user_id, revision, state_json) VALUES (?, 0, ?)")
            .bind(owner).bind(serde_json::to_string(&RouterRolloutState::default()).unwrap()).execute(&mut *tx).await.map_err(|e| e.to_string())?;
        let row = sqlx::query("SELECT revision, state_json FROM model_router_deployments WHERE user_id = ? FOR UPDATE")
            .bind(owner).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
        let actual_revision = row.get::<u64, _>("revision");
        if actual_revision != expected
            && !matches!(&change, RolloutChange::CriticalViolation { .. })
        {
            return Err("Rollout revision conflict".into());
        }
        let previous: RouterRolloutState =
            serde_json::from_str(&row.get::<String, _>("state_json")).map_err(|e| e.to_string())?;
        if previous.revision != actual_revision {
            return Err("Corrupt rollout revision".into());
        }
        // A handler's preflight may race another stop. Resolve this under the
        // deployment lock so retries preserve the winning revision and audit.
        if let RolloutChange::CriticalViolation { deployment_id } = &change
            && previous.deployment.as_ref().is_some_and(|d| {
                d.deployment_id == *deployment_id && d.mode == RolloutMode::RolledBack
            })
        {
            tx.rollback().await.map_err(|e| e.to_string())?;
            connection.release();
            return Ok(previous);
        }
        let operation = match &change {
            RolloutChange::Publish(d) => {
                serde_json::json!({"type":"publish_shadow", "candidate_sha256":d.tuning.candidate_sha256, "protocol_sha256":d.tuning.protocol_sha256, "review":d.review})
            }
            RolloutChange::Canary { basis_points } => {
                serde_json::json!({"type":"canary", "basis_points":basis_points})
            }
            RolloutChange::Rollback { reason } => {
                serde_json::json!({"type":"rollback", "reason":reason})
            }
            RolloutChange::CriticalViolation { deployment_id } => {
                serde_json::json!({"type":"critical_violation", "deployment_id":deployment_id})
            }
            RolloutChange::Revoke { source_ids } => {
                serde_json::json!({"type":"revoke", "source_ids":source_ids})
            }
        };
        let next = transition(previous, owner, change, Utc::now())?;
        sqlx::query("UPDATE model_router_deployments SET revision = ?, state_json = ? WHERE user_id = ? AND revision = ?")
            .bind(next.revision).bind(serde_json::to_string(&next).map_err(|e| e.to_string())?).bind(owner).bind(actual_revision)
            .execute(&mut *tx).await.map_err(|e| e.to_string())?;
        let details = serde_json::json!({"owner_id":owner,"revision":next.revision,"deployment_id":next.deployment.as_ref().map(|d| &d.deployment_id),"mode":next.deployment.as_ref().map(|d| d.mode),"state_sha256":content_sha256(&next)?,"operation":operation});
        sqlx::query("INSERT INTO auth_audit_logs (log_id, user_id, action, resource_type, resource_id, details) VALUES (?, ?, 'router_rollout', 'model_router', ?, ?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(actor).bind(next.deployment.as_ref().map(|d| &d.deployment_id)).bind(details.to_string())
            .execute(&mut *tx).await.map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        connection.release();
        Ok(next)
    }
}

pub fn candidate_json_sha256(json: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(json.as_bytes()))
}

pub const OUTCOME_EVENT: &str = "model_router_reviewed_outcome";
pub const OUTCOME_KEY: &str = "model-router-reviewed-outcome-v1";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterReviewedOutcome {
    pub rubric_version: String,
    pub evidence_reference: String,
    pub acceptable: Option<bool>,
    pub corrected: Option<bool>,
    /// Operator-reviewed full episode price, including judge, retries and failures.
    pub full_episode_cost_usd: Option<f64>,
    pub episode_latency_ms: Option<u64>,
    pub critical_violation: bool,
}
impl RouterReviewedOutcome {
    pub fn validate(&self) -> Result<(), String> {
        if !opaque(&self.rubric_version)
            || !opaque(&self.evidence_reference)
            || self
                .full_episode_cost_usd
                .is_some_and(|n| !n.is_finite() || !(0.0..=1e9).contains(&n))
        {
            return Err("Invalid reviewed outcome".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewedOutcomeRecord {
    pub deployment_id: String,
    pub reviewed_by: String,
    pub reviewed_at: DateTime<Utc>,
    pub outcome: RouterReviewedOutcome,
}
#[derive(Clone, Debug)]
pub struct RolloutRun {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    pub selected_offering_id: String,
    pub economy_offering_id: String,
    pub reason: astra_turn_types::model_routing::ModelRoutingReason,
    pub rollout: RouterRolloutDecision,
    pub outcome: Option<ReviewedOutcomeRecord>,
}
#[derive(Default, Serialize)]
pub struct RolloutCohortMetrics {
    pub observed_runs: usize,
    pub sessions: usize,
    pub completed: usize,
    pub economy_selections: usize,
    pub abstentions: usize,
    pub disagreements: usize,
    pub admission_failures: usize,
    pub routing_failures: usize,
    pub known_quality: usize,
    pub acceptable: usize,
    pub known_corrections: usize,
    pub corrections: usize,
    pub known_cost: usize,
    pub known_latency: usize,
    pub total_known_cost_usd: f64,
    pub cost_per_acceptable_task: Option<f64>,
    pub p95_episode_latency_ms: Option<u64>,
    pub p95_routing_overhead_us: Option<u64>,
    pub critical_violations: usize,
}
#[derive(Serialize)]
pub struct RouterRolloutDashboard {
    pub revision: u64,
    pub deployment_id: Option<String>,
    pub mode: Option<RolloutMode>,
    pub candidate_sha256: Option<String>,
    pub policy_revision: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub truncated: bool,
    pub runs: usize,
    pub cohorts: std::collections::BTreeMap<String, RolloutCohortMetrics>,
    pub production_qualified: bool,
}
fn p95(mut values: Vec<u64>) -> Option<u64> {
    values.sort_unstable();
    values
        .get((values.len() * 95).div_ceil(100).saturating_sub(1))
        .copied()
}
/// Reports the first admitted run per session (selected before outcomes). This
/// avoids weighting long/corrected sessions more heavily or dropping failures.
pub fn dashboard(
    state: &RouterRolloutState,
    runs: &[RolloutRun],
    truncated: bool,
) -> RouterRolloutDashboard {
    let mut result = RouterRolloutDashboard {
        revision: state.revision,
        deployment_id: state.deployment.as_ref().map(|d| d.deployment_id.clone()),
        mode: state.deployment.as_ref().map(|d| d.mode),
        candidate_sha256: state
            .deployment
            .as_ref()
            .map(|d| d.tuning.candidate_sha256.clone()),
        policy_revision: state.deployment.as_ref().map(|d| d.policy_revision.clone()),
        expires_at: state.deployment.as_ref().map(|d| d.expires_at),
        truncated,
        runs: runs.len(),
        cohorts: Default::default(),
        production_qualified: false,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut overhead = std::collections::BTreeMap::<String, Vec<u64>>::new();
    let mut latencies = std::collections::BTreeMap::<String, Vec<u64>>::new();
    for run in runs {
        let r = &run.rollout;
        if Some(&r.deployment_id) != result.deployment_id.as_ref() {
            continue;
        }
        let key = match r.cohort {
            RolloutCohort::Shadow => "shadow",
            RolloutCohort::Control => "control",
            RolloutCohort::Treatment => "treatment",
        }
        .to_string();
        let m = result.cohorts.entry(key.clone()).or_default();
        // Operational failures must remain visible even on later turns in a
        // session. Only statistical outcome comparisons use representatives.
        m.observed_runs += 1;
        m.admission_failures += usize::from(r.admission_rejected);
        m.routing_failures += usize::from(r.routing_failure.is_some());
        m.critical_violations += usize::from(
            run.outcome
                .as_ref()
                .is_some_and(|o| o.outcome.critical_violation),
        );
        overhead
            .entry(key.clone())
            .or_default()
            .push(r.routing_overhead_us);
        // Shadow and canary are distinct epochs; a session may appear in both.
        if !seen.insert((r.cohort == RolloutCohort::Shadow, &run.session_id)) {
            continue;
        }
        m.sessions += 1;
        m.completed += usize::from(run.status == "completed");
        m.economy_selections += usize::from(run.selected_offering_id == run.economy_offering_id);
        m.abstentions += usize::from(r.abstained);
        m.disagreements += usize::from(r.proposed_offering_id != run.selected_offering_id);
        if let Some(outcome) = &run.outcome {
            let o = &outcome.outcome;
            m.known_quality += usize::from(o.acceptable.is_some());
            m.acceptable += usize::from(o.acceptable == Some(true));
            m.known_corrections += usize::from(o.corrected.is_some());
            m.corrections += usize::from(o.corrected == Some(true));
            m.known_cost += usize::from(o.full_episode_cost_usd.is_some());
            m.total_known_cost_usd += o.full_episode_cost_usd.unwrap_or(0.0);
            m.known_latency += usize::from(o.episode_latency_ms.is_some());
            if let Some(ms) = o.episode_latency_ms {
                latencies.entry(key).or_default().push(ms);
            }
        }
    }
    for (key, m) in &mut result.cohorts {
        m.p95_routing_overhead_us = p95(overhead.remove(key).unwrap_or_default());
        m.p95_episode_latency_ms = p95(latencies.remove(key).unwrap_or_default());
        if m.known_quality == m.sessions && m.known_cost == m.sessions && m.acceptable > 0 {
            m.cost_per_acceptable_task = Some(m.total_known_cost_usd / m.acceptable as f64);
        }
    }
    result
}
impl DatabaseRouterRolloutStore {
    pub async fn runs(
        &self,
        owner: &str,
        state: &RouterRolloutState,
    ) -> Result<(Vec<RolloutRun>, bool), String> {
        let Some(d) = &state.deployment else {
            return Ok((Vec::new(), false));
        };
        // Deployment identity owns membership; host and database clocks may
        // differ, so a publication-time filter could discard valid decisions.
        let rows = sqlx::query("SELECT e.run_id, e.session_id, JSON_UNQUOTE(JSON_EXTRACT(e.payload_json, '$.data.selected_offering_id')) AS selected_offering_id, JSON_UNQUOTE(JSON_EXTRACT(e.payload_json, '$.data.policy.economy_offering_id')) AS economy_offering_id, JSON_UNQUOTE(JSON_EXTRACT(e.payload_json, '$.data.reason')) AS reason, JSON_UNQUOTE(JSON_EXTRACT(e.payload_json, '$.data.rollout')) AS rollout_json, r.status, o.payload_json AS outcome_json FROM agent_run_events e JOIN agent_runs r ON r.user_id = e.user_id AND r.run_id = e.run_id LEFT JOIN agent_run_events o ON o.user_id = e.user_id AND o.run_id = e.run_id AND o.idempotency_key = ? WHERE e.user_id = ? AND e.event_type = ? AND JSON_UNQUOTE(JSON_EXTRACT(e.payload_json, '$.data.rollout.deployment_id')) = ? ORDER BY e.created_at, e.run_id LIMIT 10001")
            .bind(OUTCOME_KEY).bind(owner).bind(crate::model_routing::EVENT_TYPE).bind(&d.deployment_id)
            .fetch_all(self.0.get()).await.map_err(|e| e.to_string())?;
        let truncated = rows.len() > 10_000;
        let mut runs = Vec::new();
        for row in rows.into_iter().take(10_000) {
            let rollout = serde_json::from_str(&row.get::<String, _>("rollout_json"))
                .map_err(|e| e.to_string())?;
            let reason = serde_json::from_value(serde_json::Value::String(row.get("reason")))
                .map_err(|e| e.to_string())?;
            let outcome = row
                .get::<Option<String>, _>("outcome_json")
                .map(|json| {
                    let value: serde_json::Value =
                        serde_json::from_str(&json).map_err(|e| e.to_string())?;
                    serde_json::from_value(value["data"].clone()).map_err(|e| e.to_string())
                })
                .transpose()?;
            runs.push(RolloutRun {
                run_id: row.get("run_id"),
                session_id: row.get("session_id"),
                status: row.get("status"),
                selected_offering_id: row.get("selected_offering_id"),
                economy_offering_id: row.get("economy_offering_id"),
                reason,
                rollout,
                outcome,
            });
        }
        Ok((runs, truncated))
    }
}
