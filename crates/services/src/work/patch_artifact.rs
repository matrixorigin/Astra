use super::{
    GraphRevision, InternalSessionId, WorkBranchId, WorkBranchRevision, WorkBranchSubjectRevision,
    WorkChangeRef, WorkContentHash, WorkDomainError, WorkId, WorkOwnerId, WorkSubjectRef,
};
use chrono::{DateTime, Utc};
use serde::Serialize;

pub const WORK_PATCH_ARTIFACT_SCHEMA_VERSION: u16 = 1;
pub const WORK_PATCH_ARTIFACT_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const WORK_PATCH_ARTIFACT_MAX_LINES: u64 = 500_000;
pub const WORK_PATCH_ARTIFACT_PAGE_MAX_ITEMS: u16 = 50;

pub fn work_patch_line_count(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|byte| **byte == b'\n').count() as u64
        + u64::from(bytes.last() != Some(&b'\n'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkPatchArtifactPageLimit(u16);

impl WorkPatchArtifactPageLimit {
    pub fn new(value: u16) -> Result<Self, WorkDomainError> {
        if value == 0 || value > WORK_PATCH_ARTIFACT_PAGE_MAX_ITEMS {
            return Err(WorkDomainError::InvalidPatchArtifactPageLimit {
                value,
                maximum: WORK_PATCH_ARTIFACT_PAGE_MAX_ITEMS,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkPatchArtifactId(String);

impl WorkPatchArtifactId {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        super::validate_resource_identity("work_patch_artifact_id", &value, 64)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkProviderInvocationRef(String);

impl WorkProviderInvocationRef {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        super::validate_identity("work_provider_invocation_ref", &value, 128)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPatchFormat {
    UnifiedDiffV1,
}

impl WorkPatchFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::UnifiedDiffV1 => "unified_diff_v1",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "unified_diff_v1" => Some(Self::UnifiedDiffV1),
            _ => None,
        }
    }
}

/// Binds one immutable, already-produced patch payload to the exact Work
/// branch subject from which it was exported. The provider invocation owns
/// generation; this command only admits typed, content-addressed output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWorkPatchArtifact {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub patch_artifact_id: WorkPatchArtifactId,
    pub payload_artifact_id: String,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_graph_revision: GraphRevision,
    pub expected_subject_record_revision: WorkBranchSubjectRevision,
    pub subject_ref: WorkSubjectRef,
    pub base_subject_revision: WorkContentHash,
    pub result_subject_revision: WorkContentHash,
    pub format: WorkPatchFormat,
    pub provider_invocation_ref: WorkProviderInvocationRef,
    pub source_ref: WorkChangeRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchArtifact {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub patch_artifact_id: WorkPatchArtifactId,
    #[serde(skip_serializing)]
    pub session_id: InternalSessionId,
    #[serde(skip_serializing)]
    pub payload_artifact_id: String,
    pub source_branch_revision: WorkBranchRevision,
    pub source_graph_revision: GraphRevision,
    #[serde(skip_serializing)]
    pub source_subject_record_revision: WorkBranchSubjectRevision,
    #[serde(skip_serializing)]
    pub subject_ref: WorkSubjectRef,
    pub base_subject_revision: WorkContentHash,
    pub result_subject_revision: WorkContentHash,
    pub payload_hash: WorkContentHash,
    pub payload_bytes: u64,
    pub format: WorkPatchFormat,
    pub provider_invocation_ref: WorkProviderInvocationRef,
    pub source_ref: WorkChangeRef,
    pub created_at: DateTime<Utc>,
}

/// The reviewable bytes behind an immutable Work patch artifact. This stays
/// behind the Work repository boundary so callers never need the backing
/// session identity or artifact-store schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchArtifactContent {
    pub artifact: WorkPatchArtifact,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchArtifactCursor {
    pub created_at: DateTime<Utc>,
    pub patch_artifact_id: WorkPatchArtifactId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchArtifactQuery {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub before: Option<WorkPatchArtifactCursor>,
    pub limit: WorkPatchArtifactPageLimit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchArtifactPage {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub artifacts: Vec<WorkPatchArtifact>,
    pub next_cursor: Option<WorkPatchArtifactCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkPatchArtifactBasisResource {
    PatchIdentity,
    BranchRevision,
    GraphRevision,
    Subject,
    PayloadArtifact,
    PayloadContract,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn patch_id_is_a_bounded_resource_identity() {
        assert!(WorkPatchArtifactId::parse("patch-123").is_ok());
        assert!(WorkPatchArtifactId::parse("../patch").is_err());
        assert!(WorkPatchArtifactId::parse("p".repeat(65)).is_err());
    }

    #[test]
    fn invocation_ref_is_opaque_but_bounded() {
        assert!(WorkProviderInvocationRef::parse("edge://provider/invocation@1").is_ok());
        assert!(WorkProviderInvocationRef::parse("contains whitespace").is_err());
        assert!(WorkProviderInvocationRef::parse("i".repeat(129)).is_err());
    }

    #[test]
    fn patch_page_limit_is_bounded_independently_of_work_age() {
        assert!(WorkPatchArtifactPageLimit::new(1).is_ok());
        assert!(WorkPatchArtifactPageLimit::new(WORK_PATCH_ARTIFACT_PAGE_MAX_ITEMS).is_ok());
        assert!(WorkPatchArtifactPageLimit::new(0).is_err());
        assert!(WorkPatchArtifactPageLimit::new(WORK_PATCH_ARTIFACT_PAGE_MAX_ITEMS + 1).is_err());
    }

    #[test]
    fn patch_line_count_handles_terminal_newlines_without_phantom_rows() {
        assert_eq!(work_patch_line_count(b""), 0);
        assert_eq!(work_patch_line_count(b"one"), 1);
        assert_eq!(work_patch_line_count(b"one\n"), 1);
        assert_eq!(work_patch_line_count(b"one\ntwo"), 2);
    }

    #[test]
    fn public_patch_projection_never_serializes_internal_session_identity() {
        let hash = || {
            WorkContentHash::parse(format!("sha256:{}", "a".repeat(64))).expect("canonical hash")
        };
        let artifact = WorkPatchArtifact {
            schema_version: WORK_PATCH_ARTIFACT_SCHEMA_VERSION,
            work_id: WorkId::parse("work-1").expect("work"),
            branch_id: WorkBranchId::parse("branch-1").expect("branch"),
            patch_artifact_id: WorkPatchArtifactId::parse("patch-1").expect("patch"),
            session_id: InternalSessionId::parse("internal-session-1").expect("session"),
            payload_artifact_id: "payload-1".into(),
            source_branch_revision: WorkBranchRevision::INITIAL,
            source_graph_revision: GraphRevision::INITIAL,
            source_subject_record_revision: WorkBranchSubjectRevision::INITIAL,
            subject_ref: WorkSubjectRef::parse("workspace://repo/main").expect("subject"),
            base_subject_revision: hash(),
            result_subject_revision: WorkContentHash::parse(format!("sha256:{}", "b".repeat(64)))
                .expect("result hash"),
            payload_hash: hash(),
            payload_bytes: 42,
            format: WorkPatchFormat::UnifiedDiffV1,
            provider_invocation_ref: WorkProviderInvocationRef::parse("invocation-1")
                .expect("invocation"),
            source_ref: WorkChangeRef::parse("event-1").expect("source"),
            created_at: Utc
                .with_ymd_and_hms(2026, 8, 2, 12, 0, 0)
                .single()
                .expect("timestamp"),
        };
        let value = serde_json::to_value(artifact).expect("serialize patch projection");
        assert!(value.get("session_id").is_none());
        assert!(value.get("payload_artifact_id").is_none());
        assert!(value.get("source_subject_record_revision").is_none());
        assert!(value.get("subject_ref").is_none());
        assert_eq!(value["patch_artifact_id"], "patch-1");
    }
}
