//! Consent-gated offline routing datasets. No database reads, inference or
//! filesystem access: callers supply explicitly approved owner-scoped evidence.
mod types;
use astra_turn_types::{AssessmentConfidence, FeedbackResponseRelation, TaskDifficulty};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
pub use types::*;

pub use astra_turn_types::model_routing::MODEL_ROUTING_FEATURE_VERSION as FEATURE_VERSION;
pub const TARGET_USE: &str = "offline_model_routing";

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn identifier(value: &str) -> Result<(), String> {
    require(
        !value.is_empty()
            && value.len() <= 255
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
        "Expected an opaque identifier; redact free-text source fields",
    )
}
pub fn content_sha256(value: &impl Serialize) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "Cannot encode routing artifact")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn reference(reference: &astra_turn_types::FeedbackResponseReference) -> Result<(), String> {
    identifier(&reference.prefix_root)?;
    require(reference.message_count > 0, "Empty canonical prefix")
}
fn authorize(
    manifest: &RouterDatasetManifest,
    auth: &RouterDataAuthorization,
    now: DateTime<Utc>,
) -> Result<(), String> {
    require(
        manifest.schema_version == 1,
        "Unsupported routing dataset schema",
    )?;
    require(
        manifest.policy_version
            == astra_turn_types::model_routing::DETERMINISTIC_ROUTING_POLICY_VERSION,
        "Unsupported logging policy; action probability is unknown",
    )?;
    for id in [
        &manifest.dataset_id,
        &manifest.owner_id,
        &manifest.redaction_version,
        &manifest.policy_revision,
        &manifest.policy_version,
        &manifest.rubric_version,
        &manifest.economy.profile_id,
        &manifest.economy.offering_id,
        &manifest.economy.contract_root,
        &manifest.strong.profile_id,
        &manifest.strong.offering_id,
        &manifest.strong.contract_root,
    ] {
        identifier(id)?;
    }
    require(
        manifest.economy.profile_id != manifest.strong.profile_id
            && manifest.economy.offering_id != manifest.strong.offering_id
            && manifest.economy.contract_root != manifest.strong.contract_root,
        "Candidate profiles must be distinct",
    )?;
    require(
        manifest.train_before < manifest.validation_before
            && manifest.validation_before < manifest.created_at
            && manifest.outcome_horizon_seconds > 0,
        "Invalid split boundaries or outcome horizon",
    )?;
    require(
        manifest.created_at <= now
            && now < manifest.expires_at
            && manifest.expires_at <= auth.expires_at,
        "Dataset authorization expired or not yet valid",
    )?;
    require(
        auth.dataset_id == manifest.dataset_id
            && auth.owner_id == manifest.owner_id
            && auth.redaction_version == manifest.redaction_version
            && auth.target_use == TARGET_USE,
        "Dataset authorization scope mismatch",
    )
}
fn source_authorized(id: &str, digest: &str, auth: &RouterDataAuthorization) -> Result<(), String> {
    require(
        auth.approved_sources.get(id).map(String::as_str) == Some(digest)
            && !auth.revoked_source_ids.iter().any(|revoked| revoked == id),
        "Source missing, changed, deleted or revoked in current authorization",
    )
}
fn lineage_not_revoked(example: &RouterExample, revoked: &BTreeSet<&str>) -> Result<(), String> {
    require(
        example.source_ids().is_disjoint(revoked),
        "Referenced routing evidence was deleted or revoked",
    )
}

enum EpisodeTimeline {
    Observed(DateTime<Utc>),
    Replay(DateTime<Utc>),
}

fn episode(
    episode: &RouterEpisode,
    profile: &ModelProfile,
    manifest: &RouterDatasetManifest,
    timeline: EpisodeTimeline,
) -> Result<(), String> {
    for id in [
        &episode.execution_id,
        &episode.profile_id,
        &episode.contract_root,
    ] {
        identifier(id)?;
    }
    require(
        episode.profile_id == profile.profile_id && episode.contract_root == profile.contract_root,
        "Episode candidate revision mismatch",
    )?;
    let valid_cutoff = match timeline {
        // Full observed episodes include admission/judge work before selection.
        EpisodeTimeline::Observed(decision_at) => {
            episode.started_at <= decision_at && decision_at <= episode.completed_at
        }
        // Counterfactual episodes cannot precede their decision-time snapshot.
        EpisodeTimeline::Replay(input_cutoff) => input_cutoff <= episode.started_at,
    };
    require(
        valid_cutoff
            && episode.started_at <= episode.completed_at
            && episode.completed_at <= manifest.created_at,
        "Invalid episode evidence timeline",
    )?;
    if let Some(r) = &episode.response_reference {
        reference(r)?;
    }
    if let Some(cost) = &episode.cost {
        identifier(&cost.pricing_revision)?;
        require(
            cost.total_usd.is_finite() && (0.0..=1_000_000_000.0).contains(&cost.total_usd),
            "Invalid episode cost",
        )?;
    }
    if let Some(q) = &episode.quality {
        identifier(&q.assessor_version)?;
        require(
            q.target_execution_id == episode.execution_id
                && q.rubric_version == manifest.rubric_version
                && !q.evidence_ids.is_empty(),
            "Quality evidence is unbound or uses a different rubric",
        )?;
        for id in &q.evidence_ids {
            identifier(id)?;
        }
        require(
            episode.completed_at <= q.assessed_at && q.assessed_at <= manifest.created_at,
            "Quality evidence precedes its response or is in the future",
        )?;
    }
    Ok(())
}
/// Unknown or late evidence never becomes success. Replay windows begin at
/// the isolated episode, since paired experiments can happen after the original
/// production turn's outcome window has closed.
pub fn verified_acceptability(
    episode: &RouterEpisode,
    horizon_seconds: u32,
    as_of: DateTime<Utc>,
) -> Option<bool> {
    let horizon = episode
        .started_at
        .checked_add_signed(Duration::seconds(horizon_seconds.into()))?;
    if horizon > as_of || episode.status != EpisodeStatus::Completed {
        return None;
    }
    let quality = episode.quality.as_ref()?;
    if quality.assessed_at > horizon {
        return None;
    }
    match quality.verdict {
        Acceptability::Acceptable => Some(true),
        Acceptability::Unacceptable => Some(false),
        Acceptability::Unknown => None,
    }
}
fn usable(episode: &RouterEpisode, manifest: &RouterDatasetManifest) -> bool {
    episode
        .cost
        .as_ref()
        .is_some_and(|cost| cost.covers_full_episode)
        && verified_acceptability(
            episode,
            manifest.outcome_horizon_seconds,
            manifest.created_at,
        )
        .is_some()
}

/// Construct immutable, prompt-free examples. Fail the entire build on a
/// governance/identity conflict; retain incomplete outcomes with explicit masks.
pub fn build_router_dataset(
    input: RouterDatasetInput,
    auth: &RouterDataAuthorization,
    now: DateTime<Utc>,
) -> Result<RouterDataset, String> {
    let manifest = input.manifest;
    authorize(&manifest, auth, now)?;
    require(
        !input.sources.is_empty() && input.sources.len() <= 100_000,
        "Dataset requires 1..100000 sources",
    )?;
    let revoked = auth.revoked_source_ids.iter().map(String::as_str).collect();
    let mut ids = BTreeSet::new();
    let mut runs = BTreeSet::new();
    let mut executions = BTreeSet::new();
    let mut splits = BTreeMap::new();
    let mut examples = Vec::new();
    for source in input.sources {
        identifier(&source.source_id)?;
        let source_digest = content_sha256(&source)?;
        source_authorized(&source.source_id, &source_digest, auth)?;
        require(source.owner_id == manifest.owner_id, "Cross-owner source")?;
        require(
            ids.insert(source.source_id.clone()),
            "Duplicate source identity",
        )?;
        let d = &source.decision;
        d.validate_identity(&d.run_id, &d.session_id)?;
        identifier(&d.run_id)?;
        identifier(&d.session_id)?;
        require(runs.insert(d.run_id.clone()), "Duplicate routing decision")?;
        require(
            d.policy_version == manifest.policy_version
                && d.policy.revision == manifest.policy_revision
                && d.policy.economy_offering_id == manifest.economy.offering_id
                && d.policy.strong_offering_id == manifest.strong.offering_id,
            "Routing policy revision mismatch",
        )?;
        let selected = if d.selected_offering_id == manifest.economy.offering_id {
            &manifest.economy
        } else {
            &manifest.strong
        };
        require(
            d.selected_contract_root == selected.contract_root,
            "Selected model contract mismatch",
        )?;
        let horizon = source
            .decision_at
            .checked_add_signed(Duration::seconds(manifest.outcome_horizon_seconds.into()))
            .ok_or("Invalid horizon")?;
        require(
            horizon <= manifest.created_at,
            "Outcome horizon has not matured",
        )?;
        if let Some(r) = &d.input_reference {
            reference(r)?;
        }
        if let Some(f) = d.features {
            require(
                f.schema_version == FEATURE_VERSION,
                "Unsupported feature version",
            )?;
            require(
                f.read_only_primary
                    == crate::model_routing::routing_read_only_primary(d.work_admission.as_ref())
                    && (!f.supported_input || d.input_reference.is_some()),
                "Structural features differ from saved admission",
            )?;
            let a = d.assessment.unwrap_or_default();
            require(
                f.assessment_present == d.assessment.is_some()
                    && f.difficulty == a.difficulty
                    && f.difficulty_confidence == a.difficulty_confidence,
                "Feature snapshot differs from decision-time assessment",
            )?;
            require(
                f.assessment_present
                    || (f.difficulty == TaskDifficulty::Unknown
                        && f.difficulty_confidence == AssessmentConfidence::Unknown),
                "Absent assessment contains inferred features",
            )?;
        }
        require(
            d.rollout
                .as_ref()
                .is_none_or(|r| r.cohort != crate::tuning::rollout::RolloutCohort::Treatment),
            "Canary treatments cannot serve as deterministic Auto baseline evidence",
        )?;
        let split = if source.decision_at < manifest.train_before {
            DatasetSplit::Train
        } else if source.decision_at < manifest.validation_before {
            DatasetSplit::Validation
        } else {
            DatasetSplit::Test
        };
        require(
            !source.group_keys.is_empty(),
            "Missing related-task/duplicate grouping",
        )?;
        let mut groups = source.group_keys.clone();
        for group in &groups {
            identifier(group)?;
        }
        // Namespaced implicit keys prevent overlapping snapshots and sessions
        // even when the exporter omitted them from the grouping attestation.
        groups = groups.into_iter().map(|g| format!("group:{g}")).collect();
        groups.push(format!("session:{}", d.session_id));
        if let Some(r) = &d.input_reference {
            groups.push(format!("input:{}:{}", r.prefix_root, r.message_count));
        }
        groups.sort();
        groups.dedup();
        for group in &groups {
            require(
                splits
                    .insert(group.clone(), split)
                    .is_none_or(|previous| previous == split),
                "Related sessions, snapshots or duplicate tasks cross dataset splits",
            )?;
        }
        if let Some(observed) = &source.observed {
            episode(
                observed,
                selected,
                &manifest,
                EpisodeTimeline::Observed(source.decision_at),
            )?;
            require(
                executions.insert(observed.execution_id.clone()),
                "Duplicate execution evidence",
            )?;
        }
        if let Some(pair) = &source.paired {
            identifier(&pair.fixture_revision)?;
            for (arm, profile) in [
                (&pair.economy, &manifest.economy),
                (&pair.strong, &manifest.strong),
            ] {
                identifier(&arm.snapshot_root)?;
                identifier(&arm.isolation_id)?;
                require(
                    d.input_reference.as_ref() == Some(&arm.input_reference),
                    "Replay does not start from the decision-time input",
                )?;
                episode(
                    &arm.episode,
                    profile,
                    &manifest,
                    EpisodeTimeline::Replay(source.decision_at),
                )?;
                require(
                    executions.insert(arm.episode.execution_id.clone()),
                    "Duplicate execution evidence",
                )?;
            }
            require(
                pair.economy.snapshot_root == pair.strong.snapshot_root,
                "Paired replay snapshots differ",
            )?;
            require(
                pair.isolation != ReplayIsolation::IsolatedSandbox
                    || pair.economy.isolation_id != pair.strong.isolation_id,
                "Paired rollouts share a mutable sandbox",
            )?;
        }
        if let Some(followup) = &source.followup {
            identifier(&followup.source_id)?;
            let observed = source
                .observed
                .as_ref()
                .ok_or("Feedback has no observed response")?;
            require(
                observed.response_reference.as_ref() == Some(&followup.response_reference)
                    && followup.assessment.feedback_relation
                        == FeedbackResponseRelation::PreviousResponse,
                "Follow-up target is unbound or ambiguous",
            )?;
            require(
                observed.completed_at < followup.observed_at && followup.observed_at <= horizon,
                "Follow-up lies outside the outcome window",
            )?;
        }
        let eligible = d.features.is_some()
            && source.paired.as_ref().is_some_and(|pair| {
                pair.both_eligible
                    && usable(&pair.economy.episode, &manifest)
                    && usable(&pair.strong.episode, &manifest)
            });
        let example = RouterExample {
            source_id: source.source_id,
            source_sha256: source_digest,
            session_id: d.session_id.clone(),
            run_id: d.run_id.clone(),
            group_keys: groups,
            split,
            decision_at: source.decision_at,
            input_reference: d.input_reference.clone(),
            features: d.features,
            selected_profile_id: selected.profile_id.clone(),
            selected_action_probability: 1.0,
            observed: source.observed,
            paired: source.paired,
            followup: source.followup,
            paired_training_eligible: eligible,
        };
        lineage_not_revoked(&example, &revoked)?;
        examples.push(example);
    }
    examples.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    let content_sha256 = content_sha256(&(&manifest, &examples))?;
    Ok(RouterDataset {
        manifest,
        content_sha256,
        examples,
    })
}

/// Export/train consumers must recheck current consent/deletion state. Artifacts
/// are invalid once any source is withdrawn; rebuilding creates a new hash.
pub fn validate_router_dataset(
    dataset: &RouterDataset,
    auth: &RouterDataAuthorization,
    now: DateTime<Utc>,
) -> Result<(), String> {
    authorize(&dataset.manifest, auth, now)?;
    require(
        content_sha256(&(&dataset.manifest, &dataset.examples))? == dataset.content_sha256,
        "Routing dataset hash mismatch",
    )?;
    let revoked = auth.revoked_source_ids.iter().map(String::as_str).collect();
    for example in &dataset.examples {
        source_authorized(&example.source_id, &example.source_sha256, auth)?;
        lineage_not_revoked(example, &revoked)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
