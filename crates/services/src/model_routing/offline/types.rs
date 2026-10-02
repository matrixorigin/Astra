//! Offline evidence contracts. IDs must be pseudonymous; no free-text prompts,
//! tool results, credentials or judge rationale are exported.
use crate::model_routing::ModelRoutingDecision;
use astra_turn_types::{
    FeedbackResponseReference, TurnAssessment, model_routing::ModelRoutingFeatures,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub profile_id: String,
    pub offering_id: String,
    pub contract_root: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterDatasetManifest {
    pub schema_version: u8,
    pub dataset_id: String,
    pub owner_id: String,
    pub redaction_version: String,
    pub policy_version: String,
    pub policy_revision: String,
    pub rubric_version: String,
    pub economy: ModelProfile,
    pub strong: ModelProfile,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Time splits are assigned before looking at outcomes.
    pub train_before: DateTime<Utc>,
    pub validation_before: DateTime<Utc>,
    pub outcome_horizon_seconds: u32,
}

/// An independently supplied, current authorization snapshot, checked on every
/// build/train. Local files are operator attestations, not server auth tokens.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterDataAuthorization {
    pub dataset_id: String,
    pub owner_id: String,
    pub target_use: String,
    pub redaction_version: String,
    pub expires_at: DateTime<Utc>,
    /// Each digest approves the complete source envelope, including nested evidence.
    pub approved_sources: std::collections::BTreeMap<String, String>,
    /// Applies to envelope IDs and every referenced feedback, execution, verifier
    /// evidence and replay snapshot ID. Revocation overrides envelope approval.
    pub revoked_source_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterDatasetInput {
    pub manifest: RouterDatasetManifest,
    pub sources: Vec<RouterTraceSource>,
}

/// Each approved source includes its entire evidence envelope. The decision must
/// be copied from the owner-scoped immutable run event, not reconstructed from
/// terminal model names. Group keys cover workspace/repository/task duplicates.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterTraceSource {
    pub source_id: String,
    pub owner_id: String,
    pub decision_at: DateTime<Utc>,
    pub decision: ModelRoutingDecision,
    pub group_keys: Vec<String>,
    pub observed: Option<RouterEpisode>,
    pub paired: Option<PairedRouterReplay>,
    pub followup: Option<RouterFollowup>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeStatus {
    Completed,
    ProviderFailure,
    /// Infrastructure prevented completing the episode. A completed episode
    /// with model-caused tool misuse should instead receive an unacceptable label.
    ToolFailure,
    Cancelled,
    NonReplayable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Acceptability {
    Acceptable,
    Unacceptable,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityAssessor {
    Human,
    ExecutableVerifier,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseQualityEvidence {
    pub target_execution_id: String,
    pub rubric_version: String,
    pub assessor: QualityAssessor,
    pub assessor_version: String,
    pub evidence_ids: Vec<String>,
    pub assessed_at: DateTime<Utc>,
    pub verdict: Acceptability,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeCost {
    /// Includes all primary/auxiliary calls, retries and failed attempts.
    pub total_usd: f64,
    pub pricing_revision: String,
    pub covers_full_episode: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterEpisode {
    pub execution_id: String,
    pub profile_id: String,
    pub contract_root: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub response_reference: Option<FeedbackResponseReference>,
    pub status: EpisodeStatus,
    pub quality: Option<ResponseQualityEvidence>,
    pub cost: Option<EpisodeCost>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayIsolation {
    ImmutableFixtures,
    IsolatedSandbox,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayArm {
    pub input_reference: FeedbackResponseReference,
    /// Covers input, environment, tool fixtures, and execution budgets.
    pub snapshot_root: String,
    pub isolation_id: String,
    pub episode: RouterEpisode,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairedRouterReplay {
    pub isolation: ReplayIsolation,
    pub fixture_revision: String,
    /// Both candidates passed the same capability/access constraints in replay.
    pub both_eligible: bool,
    pub economy: ReplayArm,
    pub strong: ReplayArm,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterFollowup {
    pub source_id: String,
    pub observed_at: DateTime<Utc>,
    pub response_reference: FeedbackResponseReference,
    pub assessment: TurnAssessment,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetSplit {
    Train,
    Validation,
    Test,
}

/// Allowlisted projection: full Work plans and prompt-bearing admission fields
/// from the durable decision are deliberately absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterExample {
    pub source_id: String,
    pub source_sha256: String,
    pub session_id: String,
    pub run_id: String,
    pub group_keys: Vec<String>,
    pub split: DatasetSplit,
    pub decision_at: DateTime<Utc>,
    pub input_reference: Option<FeedbackResponseReference>,
    pub features: Option<ModelRoutingFeatures>,
    pub selected_profile_id: String,
    /// Deterministic logging supports only the selected action, not other arms.
    pub selected_action_probability: f64,
    pub observed: Option<RouterEpisode>,
    pub paired: Option<PairedRouterReplay>,
    pub followup: Option<RouterFollowup>,
    pub paired_training_eligible: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterDataset {
    pub manifest: RouterDatasetManifest,
    pub content_sha256: String,
    pub examples: Vec<RouterExample>,
}

impl RouterEpisode {
    fn extend_source_ids<'a>(&'a self, ids: &mut std::collections::BTreeSet<&'a str>) {
        ids.insert(&self.execution_id);
        if let Some(quality) = &self.quality {
            ids.extend(quality.evidence_ids.iter().map(String::as_str));
        }
    }
}

impl RouterExample {
    /// Derive lineage from retained evidence instead of trusting a parallel,
    /// caller-supplied list that can omit dependencies.
    pub fn source_ids(&self) -> std::collections::BTreeSet<&str> {
        let mut ids = std::collections::BTreeSet::from([self.source_id.as_str()]);
        if let Some(observed) = &self.observed {
            observed.extend_source_ids(&mut ids);
        }
        if let Some(paired) = &self.paired {
            for arm in [&paired.economy, &paired.strong] {
                ids.insert(arm.snapshot_root.as_str());
                arm.episode.extend_source_ids(&mut ids);
            }
        }
        if let Some(followup) = &self.followup {
            ids.insert(followup.source_id.as_str());
        }
        ids
    }
}

impl RouterDataset {
    /// All retained sources, including examples not eligible for training.
    pub fn source_ids(&self) -> std::collections::BTreeSet<&str> {
        self.examples
            .iter()
            .flat_map(RouterExample::source_ids)
            .collect()
    }
}
