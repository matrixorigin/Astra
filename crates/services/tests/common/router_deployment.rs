use astra_services::model_routing::offline::content_sha256;
use astra_services::tuning::rollout::*;
use astra_services::tuning::*;
use chrono::Utc;

pub fn deployment(owner: &str) -> RouterDeployment {
    let now = Utc::now();
    let expires = now + chrono::Duration::days(1);
    let protocol: RouterQualificationProtocol = serde_json::from_value(serde_json::json!({
        "schema_version":1,"job_id":"test-job","owner_id":owner,"dataset_id":"dataset-1","registered_at":now,
        "training_config_sha256":"config-digest","evaluation_plan_sha256":"plan-digest",
        "minimum_test_groups":100,"minimum_stratum_groups":100,"minimum_pair_coverage":0.95,
        "maximum_quality_regression":0.01,"minimum_cost_saving_fraction":0.2,"maximum_episode_cost_usd":1.0,
        "maximum_p95_latency_ratio":1.1,"confidence":0.95,"required_strata":[]
    })).unwrap();
    RouterDeployment {
        deployment_id: uuid::Uuid::new_v4().to_string(),
        owner_id: owner.into(),
        policy_revision: "policy-1".into(),
        rubric_version: "rubric-1".into(),
        // Storage validates provenance and atomic lifecycle, not the scorer. The
        // public publication path separately rebuilds the qualified candidate.
        candidate_json: "{}".into(),
        tuning: RouterTuningRecord {
            schema_version: 1,
            job_id: protocol.job_id.clone(),
            owner_id: owner.into(),
            dataset_sha256: "dataset-digest".into(),
            candidate_sha256: candidate_json_sha256("{}"),
            protocol_sha256: content_sha256(&protocol).unwrap(),
            evaluated_at: now,
            expires_at: expires,
            source_ids: vec!["source-1".into()],
            status: RouterQualificationStatus::ReadyForShadow,
            production_qualified: false,
        },
        protocol,
        review: RolloutReview {
            online_consent_reference: "consent-1".into(),
            verifier_review_reference: "review-1".into(),
            safety_review_reference: "safety-1".into(),
            expires_at: expires,
            minimum_shadow_sessions: 10,
            maximum_routing_overhead_ms: 100,
        },
        mode: RolloutMode::Shadow,
        canary_basis_points: 0,
        assignment_salt: uuid::Uuid::new_v4().to_string(),
        created_at: now,
        expires_at: expires,
        stop_reason: None,
    }
}
