//! Shared live scoring; authentication, catalog admission and durable state are
//! supplied by their existing owners. Offline artifacts cannot self-activate.
use super::{
    offline::{CandidateChoice, RouterCandidate, RouterTrainingConfig},
    qualification::qualify_router,
};
use astra_services::{
    model_routing::offline::*,
    tuning::{RouterQualificationProtocol, RouterQualificationStatus, rollout::*},
};
use astra_turn_types::model_routing::{AutoModelRoutingPolicy, ModelRoutingFeatures};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterPublishRequest {
    pub expected_revision: u64,
    pub input: RouterDatasetInput,
    pub authorization: RouterDataAuthorization,
    pub config: RouterTrainingConfig,
    pub protocol: RouterQualificationProtocol,
    pub review: RolloutReview,
}

pub fn prepare_shadow(
    request: RouterPublishRequest,
    owner: &str,
    now: DateTime<Utc>,
) -> Result<RouterDeployment, String> {
    request.review.validate(now)?;
    if request.input.manifest.owner_id != owner {
        return Err("Qualification owner differs".into());
    }
    let revision = request.input.manifest.policy_revision.clone();
    let rubric_version = request.input.manifest.rubric_version.clone();
    let qualified = qualify_router(
        request.input,
        &request.authorization,
        now,
        request.config,
        request.protocol,
    )?;
    if qualified.tuning.status != RouterQualificationStatus::ReadyForShadow {
        return Err("Offline qualification did not pass".into());
    }
    let expires_at = qualified
        .tuning
        .expires_at
        .min(request.review.expires_at)
        .min(request.authorization.expires_at);
    Ok(RouterDeployment {
        deployment_id: uuid::Uuid::new_v4().to_string(),
        owner_id: owner.into(),
        policy_revision: revision,
        rubric_version,
        candidate_json: serde_json::to_string(&qualified.candidate).map_err(|e| e.to_string())?,
        tuning: qualified.tuning,
        protocol: qualified.protocol,
        review: request.review,
        mode: RolloutMode::Shadow,
        canary_basis_points: 0,
        assignment_salt: uuid::Uuid::new_v4().to_string(),
        created_at: now,
        expires_at,
        stop_reason: None,
    })
}

pub fn deployment_candidate(d: &RouterDeployment) -> Result<RouterCandidate, String> {
    if candidate_json_sha256(&d.candidate_json) != d.tuning.candidate_sha256 {
        return Err("Candidate digest differs".into());
    }
    let candidate: RouterCandidate =
        serde_json::from_str(&d.candidate_json).map_err(|_| "Invalid router candidate")?;
    if candidate.schema_version != 1
        || candidate.algorithm_version != "categorical-paired-outcomes-v1"
        || candidate.activation != "offline_only"
        || candidate.feature_version != FEATURE_VERSION
    {
        return Err("Unsupported router candidate version".into());
    }
    Ok(candidate)
}

/// Stable salted session assignment. Repeated turns and host restarts cannot
/// resample the cohort. The salt is operator-only and fixed for this deployment.
pub fn session_bucket(d: &RouterDeployment, session: &str) -> u16 {
    let bytes = serde_json::to_vec(&(
        "router-session-v1",
        &d.assignment_salt,
        &d.owner_id,
        session,
    ))
    .unwrap();
    let hash = Sha256::digest(bytes);
    // A 64-bit hash has negligible modulo imbalance over 10,000 buckets.
    (u64::from_be_bytes(hash[..8].try_into().unwrap()) % 10_000) as u16
}

pub fn score_live(
    state: &RouterRolloutState,
    owner: &str,
    session: &str,
    policy: &AutoModelRoutingPolicy,
    features: ModelRoutingFeatures,
    now: DateTime<Utc>,
) -> Result<Option<RouterRolloutDecision>, String> {
    let Some(d) = &state.deployment else {
        return Ok(None);
    };
    if d.mode == RolloutMode::RolledBack || d.expires_at <= now {
        return Ok(None);
    }
    if d.owner_id != owner
        || d.policy_revision != policy.revision
        || d.tuning
            .source_ids
            .iter()
            .any(|s| state.revoked_source_ids.contains(s))
    {
        return Err("Router deployment scope changed or evidence revoked".into());
    }
    let candidate = deployment_candidate(d)?;
    if candidate.economy.offering_id != policy.economy_offering_id
        || candidate.strong.offering_id != policy.strong_offering_id
    {
        return Err("Router deployment Offering pair changed".into());
    }
    let choice = if d.protocol.required_strata.contains(&features) {
        candidate.choose(features)
    } else {
        CandidateChoice::Abstain
    };
    let cohort = match d.mode {
        RolloutMode::Shadow => RolloutCohort::Shadow,
        RolloutMode::Canary if session_bucket(d, session) < d.canary_basis_points => {
            RolloutCohort::Treatment
        }
        _ => RolloutCohort::Control,
    };
    Ok(Some(RouterRolloutDecision {
        deployment_id: d.deployment_id.clone(),
        revision: state.revision,
        candidate_sha256: d.tuning.candidate_sha256.clone(),
        rubric_version: d.rubric_version.clone(),
        routing_failure: None,
        cohort,
        cohort_probability_basis_points: match cohort {
            RolloutCohort::Shadow => 10_000,
            RolloutCohort::Treatment => d.canary_basis_points,
            RolloutCohort::Control => 10_000 - d.canary_basis_points,
        },
        proposed_offering_id: if choice == CandidateChoice::Economy {
            candidate.economy.offering_id
        } else {
            candidate.strong.offering_id
        },
        abstained: choice == CandidateChoice::Abstain,
        admission_rejected: false,
        routing_overhead_us: 0,
    }))
}

pub fn validate_pinned_treatment(
    state: &RouterRolloutState,
    pinned: &RouterRolloutDecision,
    now: DateTime<Utc>,
) -> Result<(), String> {
    pinned.ensure_dispatchable()?;
    if pinned.cohort != RolloutCohort::Treatment {
        return Ok(());
    }
    let d = state
        .deployment
        .as_ref()
        .ok_or("Router deployment withdrawn")?;
    if d.mode != RolloutMode::Canary
        || d.expires_at <= now
        || d.deployment_id != pinned.deployment_id
        || d.tuning.candidate_sha256 != pinned.candidate_sha256
        || d.tuning
            .source_ids
            .iter()
            .any(|s| state.revoked_source_ids.contains(s))
    {
        return Err("Router treatment stopped, expired or revoked; start a new turn".into());
    }
    Ok(())
}
