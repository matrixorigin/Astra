//! Authenticated allocation evidence stored in the existing Session artifact owner.
use serde::{Deserialize, Serialize};

use super::{durable::EvaluationTrialBindingRecord, experiment::ExperimentSpec};

pub const WORKSPACE_ALLOCATION_ARTIFACT_KIND: &str = "evaluation_workspace_allocation";
pub const WORKSPACE_ALLOCATION_ARTIFACT_SOURCE: &str = "evaluation_edge_prepare";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationWorkspaceMaterialization {
    pub schema_version: u32,
    pub experiment_id: String,
    pub trial_id: String,
    pub run_generation: u64,
    pub spec_fingerprint: String,
    pub edge_executor_id: String,
    pub connection_generation: u64,
    pub allocation: astra_runtime_env::EvaluationAllocationReceipt,
}

impl EvaluationWorkspaceMaterialization {
    pub fn artifact_id(&self) -> String {
        use sha2::{Digest, Sha256};
        let identity = serde_json::json!([
            WORKSPACE_ALLOCATION_ARTIFACT_KIND,
            self.allocation.owner_user_id,
            self.allocation.session_id,
            self.allocation.run_id,
            self.experiment_id,
            self.trial_id,
            self.run_generation,
        ]);
        format!(
            "{:x}",
            Sha256::digest(astra_core::canonical_json_string(&identity).as_bytes())
        )
    }

    /// Persist an immutable allocation fact using the Session artifact owner.
    /// An exact retry reuses the original artifact; it never overwrites evidence.
    pub async fn persist(&self, pool: &astra_core::SharedPool) -> Result<(String, String), String> {
        use crate::{
            DatabaseSessionArtifactStore, SessionArtifactJsonRecord, SessionArtifactJsonStore,
            SessionArtifactReference, SessionArtifactReferenceKind,
        };
        let artifact_id = self.artifact_id();
        let content = serde_json::to_value(self).map_err(|error| error.to_string())?;
        let fingerprint = super::content_fingerprint(&astra_core::canonical_json_string(&content));
        let store = DatabaseSessionArtifactStore::new(astra_core::MatrixOneSettings::default())
            .with_pool(pool.clone());
        let record = SessionArtifactJsonRecord {
            artifact_id: artifact_id.clone(),
            session_id: self.allocation.session_id.clone(),
            user_id: self.allocation.owner_user_id.clone(),
            artifact_kind: WORKSPACE_ALLOCATION_ARTIFACT_KIND.into(),
            source: Some(WORKSPACE_ALLOCATION_ARTIFACT_SOURCE.into()),
            turn: None,
            round: None,
            content: content.clone(),
            metadata: Some(serde_json::json!({"content_hash": fingerprint})),
            references: vec![SessionArtifactReference {
                kind: SessionArtifactReferenceKind::Manifest,
                reference_id: format!("evaluation-trial:{}:{}", self.trial_id, self.run_generation),
            }],
        };
        if let Err(error) = store.persist_json_artifact(record).await {
            let existing = store
                .load_json_artifact(
                    &self.allocation.owner_user_id,
                    &self.allocation.session_id,
                    &artifact_id,
                )
                .await
                .map_err(|error| error.to_string())?;
            if !existing.is_some_and(|existing| {
                existing.content == content
                    && existing.artifact_kind == WORKSPACE_ALLOCATION_ARTIFACT_KIND
                    && existing.source.as_deref() == Some(WORKSPACE_ALLOCATION_ARTIFACT_SOURCE)
                    && existing.status.as_deref() == Some("active")
            }) {
                return Err(error.to_string());
            }
        }
        Ok((artifact_id, fingerprint))
    }

    pub fn validate_binding(
        &self,
        binding: &EvaluationTrialBindingRecord,
        spec: &ExperimentSpec,
    ) -> Result<(), String> {
        self.allocation.validate()?;
        let workspace = spec
            .conditions
            .workspace_execution
            .as_ref()
            .ok_or("workspace profile is absent")?;
        if self.schema_version != 1
            || self.connection_generation == 0
            || self.experiment_id != binding.experiment_id
            || self.trial_id != binding.trial_id
            || Some(self.run_generation) != binding.run_generation
            || self.spec_fingerprint != binding.spec_fingerprint
            || self.spec_fingerprint != spec.spec_fingerprint()?
            || self.allocation.owner_user_id != binding.owner_user_id
            || Some(self.allocation.session_id.as_str()) != binding.session_id.as_deref()
            || Some(self.allocation.run_id.as_str()) != binding.run_id.as_deref()
            || self.edge_executor_id != workspace.edge_executor_id
            || self.allocation.source_commit != workspace.source_commit
            || self.allocation.confinement_fingerprint != workspace.confinement.fingerprint()?
        {
            return Err(
                "workspace allocation evidence differs from the frozen trial binding".into(),
            );
        }
        Ok(())
    }
}
