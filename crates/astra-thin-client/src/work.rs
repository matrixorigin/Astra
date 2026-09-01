//! Strict public Work wire contracts shared by Rust client surfaces.
//!
//! These types describe Server responses, not local task authority. Decoding
//! rejects torn pagination and incoherent declaration/execution/delivery/check
//! facts before either the TUI or non-interactive CLI can present them.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_ITEMS: u16 = 256;
const MAX_EDGES: u16 = 1024;
const MAX_ITEM_PAGE: u16 = 8;
const MAX_EDGE_PAGE: u16 = 128;
const MAX_CRITERIA: u16 = 128;
const MAX_CAPABILITIES: usize = 16;
const MAX_SAFE_INTEGER: i64 = (1_i64 << 53) - 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskGraphPageV2 {
    pub schema_version: u16,
    pub scope: String,
    pub basis: WorkTaskGraphBasisV2,
    pub cursor: WorkTaskGraphCursorV2,
    pub next_cursor: Option<WorkTaskGraphCursorV2>,
    pub items: WorkTaskGraphItemsV2,
    pub dependencies: WorkTaskGraphDependenciesV2,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskGraphBasisV2 {
    pub work_id: String,
    pub work_revision: i64,
    pub goal_revision: i64,
    pub goal: String,
    pub criteria_set_revision: i64,
    pub criteria_member_count: u16,
    pub criteria_manifest_hash: String,
    pub branch_id: String,
    pub branch_revision: i64,
    pub branch_goal_revision: i64,
    pub branch_criteria_set_revision: i64,
    pub branch_basis_graph_revision: i64,
    pub graph_revision: i64,
    pub graph_item_count: u16,
    pub graph_edge_count: u16,
    pub graph_manifest_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskGraphCursorV2 {
    pub graph_revision: i64,
    pub item_offset: u16,
    pub dependency_offset: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskGraphItemsV2 {
    pub offset: u16,
    pub limit: u16,
    pub total: u16,
    pub entries: Vec<WorkTaskGraphItemV2>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskGraphDependenciesV2 {
    pub offset: u16,
    pub limit: u16,
    pub total: u16,
    pub entries: Vec<WorkTaskGraphDependencyV2>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskGraphItemV2 {
    pub item_id: String,
    pub revision: i64,
    pub kind: WorkTaskGraphItemKindV2,
    pub objective: String,
    pub expected_result: String,
    pub declaration_state: WorkTaskDeclarationStateV2,
    pub execution: WorkTaskExecutionV2,
    pub delivery: WorkTaskDeliveryV2,
    pub verification: WorkTaskVerificationV2,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskGraphItemKindV2 {
    Milestone,
    Task,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskDeclarationStateV2 {
    Active,
    Superseded,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemExecutionStatusV2 {
    NotStarted,
    Running,
    Waiting,
    Paused,
    Completed,
    Delegated,
    Failed,
    Cancelled,
}

impl WorkItemExecutionStatusV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Delegated => "delegated",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Delegated | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskExecutionV2 {
    pub status: WorkItemExecutionStatusV2,
    pub terminal: bool,
    pub run: Option<WorkTaskRunV2>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskRunV2 {
    pub run_id: String,
    pub attempt_id: String,
    pub graph_revision: i64,
    pub run_generation: u64,
    pub last_event_idx: i64,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemDeliveryStatusV2 {
    Unreported,
    Delivered,
    Blocked,
    Failed,
}

impl WorkItemDeliveryStatusV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unreported => "unreported",
            Self::Delivered => "delivered",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemDeliveryBlockerKindV2 {
    CapabilityUnavailable,
    DependencyBlocked,
    PolicyBlocked,
    ExternalUnavailable,
}

impl WorkItemDeliveryBlockerKindV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::DependencyBlocked => "dependency_blocked",
            Self::PolicyBlocked => "policy_blocked",
            Self::ExternalUnavailable => "external_unavailable",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskDeliveryV2 {
    pub status: WorkItemDeliveryStatusV2,
    pub summary: Option<String>,
    pub blocker_kind: Option<WorkItemDeliveryBlockerKindV2>,
    pub unavailable_capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemVerificationStatusV2 {
    Unknown,
    EvidenceAvailable,
    StaleEvidence,
}

impl WorkItemVerificationStatusV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::EvidenceAvailable => "evidence_available",
            Self::StaleEvidence => "stale_evidence",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskVerificationV2 {
    pub status: WorkItemVerificationStatusV2,
    pub latest_check: Option<WorkTaskCheckV2>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskCheckV2 {
    pub check_run_id: String,
    pub criterion: WorkTaskCriterionRefV2,
    pub criterion_set_revision: i64,
    pub graph_revision: i64,
    pub verifier_kind: WorkTaskVerifierKindV2,
    pub outcome: WorkTaskCheckOutcomeV2,
    pub coverage: WorkTaskCheckCoverageV2,
    pub subject_revision: String,
    pub evidence_ref_count: u16,
    pub produced_at: String,
    pub expires_at: Option<String>,
    pub freshness: WorkTaskCheckFreshnessV2,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskCriterionRefV2 {
    pub criterion_id: String,
    pub revision: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskVerifierKindV2 {
    Command,
    Test,
}

impl WorkTaskVerifierKindV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Test => "test",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskCheckOutcomeV2 {
    Passed,
    Failed,
    Error,
    Cancelled,
}

impl WorkTaskCheckOutcomeV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskCheckCoverageV2 {
    Complete,
    Partial,
    Unavailable,
}

impl WorkTaskCheckCoverageV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskCheckFreshnessV2 {
    Current,
    CriteriaChanged,
    GraphChanged,
    SubjectUnavailable,
    SubjectChanged,
    Expired,
}

impl WorkTaskCheckFreshnessV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::CriteriaChanged => "criteria_changed",
            Self::GraphChanged => "graph_changed",
            Self::SubjectUnavailable => "subject_unavailable",
            Self::SubjectChanged => "subject_changed",
            Self::Expired => "expired",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskGraphDependencyV2 {
    pub predecessor_item_id: String,
    pub successor_item_id: String,
    pub kind: WorkTaskGraphDependencyKindV2,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskGraphDependencyKindV2 {
    Dependency,
}

impl WorkTaskGraphPageV2 {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 2 || self.scope != "declared_work" {
            return Err("Task Graph has an unsupported schema or scope".into());
        }
        self.basis.validate()?;
        self.cursor.validate()?;
        if self.cursor.graph_revision != self.basis.graph_revision
            || self.items.offset != self.cursor.item_offset
            || self.dependencies.offset != self.cursor.dependency_offset
            || self.items.limit == 0
            || self.items.limit > MAX_ITEM_PAGE
            || self.dependencies.limit == 0
            || self.dependencies.limit > MAX_EDGE_PAGE
            || self.items.total != self.basis.graph_item_count
            || self.dependencies.total != self.basis.graph_edge_count
            || self.items.entries.len() > usize::from(self.items.limit)
            || self.dependencies.entries.len() > usize::from(self.dependencies.limit)
        {
            return Err("Task Graph page violates its pinned bounded basis".into());
        }
        let item_end = usize::from(self.items.offset) + self.items.entries.len();
        let dependency_end =
            usize::from(self.dependencies.offset) + self.dependencies.entries.len();
        if item_end > usize::from(self.items.total)
            || dependency_end > usize::from(self.dependencies.total)
            || (self.items.offset < self.items.total && self.items.entries.is_empty())
            || (self.dependencies.offset < self.dependencies.total
                && self.dependencies.entries.is_empty())
        {
            return Err("Task Graph page does not advance within its declared totals".into());
        }
        for item in &self.items.entries {
            item.validate(self.basis.graph_revision)?;
        }
        if self
            .items
            .entries
            .windows(2)
            .any(|pair| pair[0].item_id >= pair[1].item_id)
        {
            return Err("Task Graph items are not in canonical order".into());
        }
        for dependency in &self.dependencies.entries {
            validate_resource_identity("dependency predecessor", &dependency.predecessor_item_id)?;
            validate_resource_identity("dependency successor", &dependency.successor_item_id)?;
            if dependency.predecessor_item_id == dependency.successor_item_id {
                return Err("Task Graph dependency cannot be a self edge".into());
            }
        }
        if self.dependencies.entries.windows(2).any(|pair| {
            (&pair[0].predecessor_item_id, &pair[0].successor_item_id)
                >= (&pair[1].predecessor_item_id, &pair[1].successor_item_id)
        }) {
            return Err("Task Graph dependencies are not in canonical order".into());
        }
        let has_more = item_end < usize::from(self.items.total)
            || dependency_end < usize::from(self.dependencies.total);
        match self.next_cursor {
            Some(next)
                if has_more
                    && next.graph_revision == self.basis.graph_revision
                    && usize::from(next.item_offset) == item_end
                    && usize::from(next.dependency_offset) == dependency_end =>
            {
                next.validate()
            }
            None if !has_more => Ok(()),
            _ => Err("Task Graph continuation cursor is not exact".into()),
        }
    }
}

impl WorkTaskGraphBasisV2 {
    fn validate(&self) -> Result<(), String> {
        validate_resource_identity("work_id", &self.work_id)?;
        validate_resource_identity("branch_id", &self.branch_id)?;
        validate_text("goal", &self.goal, 16 * 1024)?;
        validate_positive("work_revision", self.work_revision)?;
        validate_positive("goal_revision", self.goal_revision)?;
        validate_positive("criteria_set_revision", self.criteria_set_revision)?;
        validate_positive("branch_revision", self.branch_revision)?;
        validate_positive("branch_goal_revision", self.branch_goal_revision)?;
        validate_positive(
            "branch_criteria_set_revision",
            self.branch_criteria_set_revision,
        )?;
        validate_positive(
            "branch_basis_graph_revision",
            self.branch_basis_graph_revision,
        )?;
        validate_positive("graph_revision", self.graph_revision)?;
        validate_content_hash("criteria_manifest_hash", &self.criteria_manifest_hash)?;
        validate_content_hash("graph_manifest_hash", &self.graph_manifest_hash)?;
        if self.criteria_member_count > MAX_CRITERIA
            || self.graph_item_count > MAX_ITEMS
            || self.graph_edge_count > MAX_EDGES
            || usize::from(self.graph_edge_count)
                > usize::from(self.graph_item_count)
                    .saturating_mul(usize::from(self.graph_item_count).saturating_sub(1))
                    / 2
            || self.branch_goal_revision > self.goal_revision
            || self.branch_criteria_set_revision > self.criteria_set_revision
            || self.graph_revision < self.branch_basis_graph_revision
        {
            return Err("Task Graph basis contains incoherent revisions or counts".into());
        }
        Ok(())
    }
}

impl WorkTaskGraphCursorV2 {
    fn validate(self) -> Result<(), String> {
        validate_positive("cursor.graph_revision", self.graph_revision)?;
        if self.item_offset > MAX_ITEMS || self.dependency_offset > MAX_EDGES {
            return Err("Task Graph cursor exceeds bounded totals".into());
        }
        Ok(())
    }
}

impl WorkTaskGraphItemV2 {
    fn validate(&self, current_graph_revision: i64) -> Result<(), String> {
        validate_resource_identity("item_id", &self.item_id)?;
        validate_positive("item revision", self.revision)?;
        validate_text("item objective", &self.objective, 8 * 1024)?;
        validate_text("item expected result", &self.expected_result, 8 * 1024)?;
        if self.execution.terminal != self.execution.status.is_terminal()
            || (self.execution.status == WorkItemExecutionStatusV2::NotStarted)
                != self.execution.run.is_none()
        {
            return Err("Task Graph execution status, terminal, and Run disagree".into());
        }
        if let Some(run) = &self.execution.run {
            validate_resource_identity("run_id", &run.run_id)?;
            validate_resource_identity("attempt_id", &run.attempt_id)?;
            validate_positive("run graph revision", run.graph_revision)?;
            validate_timestamp("run updated_at", &run.updated_at)?;
            // `attempt_id` identifies the immutable Work-item assignment;
            // `run_id` identifies the execution carrier that performed it.
            // A primary session can execute several assignments over its
            // lifetime, so these are deliberately independent identities.
            // Requiring equality rejects a valid graph after the first
            // primary-session settlement and makes every consumer lose the
            // entire Task Board rather than one optional field.
            if run.graph_revision > current_graph_revision
                || run.last_event_idx < -1
                || run.last_event_idx > MAX_SAFE_INTEGER
                || run.run_generation > MAX_SAFE_INTEGER as u64
            {
                return Err("Task Graph Run contains out-of-range attempt state".into());
            }
        }
        self.delivery.validate()?;
        self.verification
            .validate(self.execution.run.is_some(), current_graph_revision)
    }
}

impl WorkTaskDeliveryV2 {
    fn validate(&self) -> Result<(), String> {
        if self.unavailable_capabilities.len() > MAX_CAPABILITIES {
            return Err("Task delivery exceeds its capability bound".into());
        }
        let mut capabilities = BTreeSet::new();
        for capability in &self.unavailable_capabilities {
            validate_resource_identity("unavailable capability", capability)?;
            if !capabilities.insert(capability) {
                return Err("Task delivery contains duplicate capabilities".into());
            }
        }
        let has_summary = self
            .summary
            .as_deref()
            .is_some_and(|summary| validate_text("delivery summary", summary, 8 * 1024).is_ok());
        let coherent = match self.status {
            WorkItemDeliveryStatusV2::Unreported => {
                self.summary.is_none()
                    && self.blocker_kind.is_none()
                    && self.unavailable_capabilities.is_empty()
            }
            WorkItemDeliveryStatusV2::Delivered | WorkItemDeliveryStatusV2::Failed => {
                has_summary
                    && self.blocker_kind.is_none()
                    && self.unavailable_capabilities.is_empty()
            }
            WorkItemDeliveryStatusV2::Blocked => {
                has_summary
                    && self.blocker_kind.is_some()
                    && (self.blocker_kind
                        == Some(WorkItemDeliveryBlockerKindV2::CapabilityUnavailable))
                        == !self.unavailable_capabilities.is_empty()
            }
        };
        coherent
            .then_some(())
            .ok_or_else(|| "Task delivery facts are incoherent".into())
    }
}

impl WorkTaskVerificationV2 {
    fn validate(&self, has_run: bool, current_graph_revision: i64) -> Result<(), String> {
        let freshness = self.latest_check.as_ref().map(|check| check.freshness);
        let shape_is_valid = match self.status {
            WorkItemVerificationStatusV2::Unknown => self.latest_check.is_none(),
            WorkItemVerificationStatusV2::EvidenceAvailable => {
                freshness == Some(WorkTaskCheckFreshnessV2::Current)
            }
            WorkItemVerificationStatusV2::StaleEvidence => {
                freshness.is_some_and(|value| value != WorkTaskCheckFreshnessV2::Current)
            }
        };
        if !shape_is_valid {
            return Err("Task verification status disagrees with its latest Check".into());
        }
        let Some(check) = &self.latest_check else {
            return Ok(());
        };
        validate_resource_identity("check_run_id", &check.check_run_id)?;
        validate_resource_identity("criterion_id", &check.criterion.criterion_id)?;
        validate_positive("criterion revision", check.criterion.revision)?;
        validate_positive("criterion set revision", check.criterion_set_revision)?;
        validate_positive("check graph revision", check.graph_revision)?;
        validate_content_hash("check subject revision", &check.subject_revision)?;
        let produced_at = validate_timestamp("check produced_at", &check.produced_at)?;
        if let Some(expires_at) = check.expires_at.as_deref() {
            let expires_at = validate_timestamp("check expires_at", expires_at)?;
            if expires_at <= produced_at {
                return Err("Task Check expiry does not follow production".into());
            }
        }
        if !has_run
            || check.graph_revision > current_graph_revision
            || (check.outcome == WorkTaskCheckOutcomeV2::Passed
                && (check.coverage != WorkTaskCheckCoverageV2::Complete
                    || check.evidence_ref_count == 0))
            || (check.outcome == WorkTaskCheckOutcomeV2::Failed && check.evidence_ref_count == 0)
            || check.evidence_ref_count > 32
        {
            return Err("Task Check lacks an admissible attempt or coherent evidence".into());
        }
        Ok(())
    }
}

fn validate_positive(field: &str, value: i64) -> Result<(), String> {
    (value > 0 && value <= MAX_SAFE_INTEGER)
        .then_some(())
        .ok_or_else(|| format!("{field} must be positive"))
}

fn validate_text(field: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    (!value.trim().is_empty() && value.len() <= max_bytes)
        .then_some(())
        .ok_or_else(|| format!("{field} must be non-empty and at most {max_bytes} bytes"))
}

fn validate_resource_identity(field: &str, value: &str) -> Result<(), String> {
    (!value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().count() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    .then_some(())
    .ok_or_else(|| format!("{field} is not a canonical resource identity"))
}

fn validate_content_hash(field: &str, value: &str) -> Result<(), String> {
    let digest = value.strip_prefix("sha256:");
    digest
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .then_some(())
        .ok_or_else(|| format!("{field} is not a full SHA-256 content hash"))
}

fn validate_timestamp(field: &str, value: &str) -> Result<DateTime<FixedOffset>, String> {
    if !value.ends_with('Z') || value.as_bytes().get(10) != Some(&b'T') {
        return Err(format!("{field} is not an RFC 3339 UTC timestamp"));
    }
    DateTime::parse_from_rfc3339(value)
        .map_err(|_| format!("{field} is not an RFC 3339 UTC timestamp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> WorkTaskGraphPageV2 {
        serde_json::from_str(include_str!(
            "../../../fixtures/contracts/work_task_graph_v2.json"
        ))
        .expect("shared Task Graph v2 fixture")
    }

    #[test]
    fn shared_v2_fixture_is_valid_for_every_rust_client_surface() {
        fixture().validate().expect("valid shared fixture");
    }

    #[test]
    fn distinct_execution_carrier_and_attempt_are_valid_but_invalid_bounds_fail_closed() {
        let mut page = fixture();
        let run = page.items.entries[0]
            .execution
            .run
            .as_mut()
            .expect("fixture has an execution run");
        assert_ne!(run.run_id, run.attempt_id);
        page.validate()
            .expect("execution carrier and Work-item attempt are independent identities");

        page.items.entries[0]
            .execution
            .run
            .as_mut()
            .expect("fixture has an execution run")
            .last_event_idx = -2;
        assert!(
            page.validate()
                .unwrap_err()
                .contains("out-of-range attempt state")
        );

        let mut page = fixture();
        page.items.entries[0].delivery = WorkTaskDeliveryV2 {
            status: WorkItemDeliveryStatusV2::Blocked,
            summary: Some("Capability unavailable".into()),
            blocker_kind: Some(WorkItemDeliveryBlockerKindV2::CapabilityUnavailable),
            unavailable_capabilities: Vec::new(),
        };
        assert!(page.validate().unwrap_err().contains("delivery facts"));
    }
}
