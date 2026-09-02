use super::{
    CheckCoverage, CheckOutcome, CheckRunId, CheckVerifierKind, CriterionRevisionRef,
    CriterionSetRevision, GoalRevision, GraphRevision, InternalSessionId, WorkAttemptExecutionMode,
    WorkAttemptTerminalCut, WorkBranchId, WorkBranchRevision, WorkContentHash, WorkGoal,
    WorkGraphItemChange, WorkId, WorkItemDeclarationState, WorkItemEdge, WorkItemId, WorkItemKind,
    WorkItemRevision, WorkItemRevisionRef, WorkItemText, WorkRevision,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const WORK_PLAN_CONTEXT_SCHEMA_VERSION: u16 = 1;
const WORK_TASK_GRAPH_SCHEMA_VERSION: u16 = 2;
pub const WORK_TASK_GRAPH_ITEM_PAGE_MAX_ITEMS: u16 = 8;
pub const WORK_TASK_GRAPH_DEPENDENCY_PAGE_MAX_ITEMS: u16 = 128;

/// The constant-size identity needed to attach one internal session to its
/// canonical Work branch.
///
/// Runtime admission deliberately loads this projection instead of a full
/// [`WorkPlanContext`]. Plan item text belongs on the explicit planning-tool
/// path, not on every turn's request-validation path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkSessionPlanBinding {
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub graph_revision: GraphRevision,
}

/// Constant-size server-internal binding resolved from a public Work branch.
///
/// Public clients identify `work_id + branch_id`; only the runtime needs the
/// opaque conversation identity used to execute the branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchRuntimeBinding {
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub branch_revision: WorkBranchRevision,
    pub session_id: InternalSessionId,
    pub graph_revision: GraphRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPlanBasis {
    pub work_id: WorkId,
    pub work_revision: WorkRevision,
    pub goal_revision: GoalRevision,
    pub goal: WorkGoal,
    pub criteria_set_revision: CriterionSetRevision,
    pub criteria_member_count: u16,
    pub criteria_manifest_hash: WorkContentHash,
    pub branch_id: WorkBranchId,
    pub branch_revision: WorkBranchRevision,
    pub branch_goal_revision: GoalRevision,
    pub branch_criteria_set_revision: CriterionSetRevision,
    pub branch_basis_graph_revision: GraphRevision,
    pub graph_revision: GraphRevision,
    pub graph_item_count: u16,
    pub graph_edge_count: u16,
    pub graph_manifest_hash: WorkContentHash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPlanItem {
    pub item_id: WorkItemId,
    pub revision: WorkItemRevision,
    pub kind: WorkItemKind,
    pub objective: WorkItemText,
    pub expected_result: WorkItemText,
    pub declaration_state: WorkItemDeclarationState,
}

impl WorkPlanItem {
    pub(crate) fn revision_ref(&self) -> WorkItemRevisionRef {
        WorkItemRevisionRef {
            item_id: self.item_id.clone(),
            revision: self.revision,
        }
    }
}

/// Exact, owner-scoped request for one bounded slice of the current declared
/// Task Graph. Non-zero offsets are valid only when pinned to the graph
/// revision returned by the previous page, so a replan cannot silently splice
/// two revisions into one client view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkTaskGraphQuery {
    pub(crate) owner_id: super::WorkOwnerId,
    pub(crate) work_id: WorkId,
    pub(crate) branch_id: WorkBranchId,
    pub(crate) expected_graph_revision: Option<GraphRevision>,
    pub(crate) item_offset: u16,
    pub(crate) item_limit: u16,
    pub(crate) dependency_offset: u16,
    pub(crate) dependency_limit: u16,
}

impl WorkTaskGraphQuery {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner_id: super::WorkOwnerId,
        work_id: WorkId,
        branch_id: WorkBranchId,
        expected_graph_revision: Option<GraphRevision>,
        item_offset: u16,
        item_limit: u16,
        dependency_offset: u16,
        dependency_limit: u16,
    ) -> Result<Self, super::WorkDomainError> {
        if item_limit == 0 || item_limit > WORK_TASK_GRAPH_ITEM_PAGE_MAX_ITEMS {
            return Err(super::WorkDomainError::InvalidTaskGraphPageLimit {
                max_items: WORK_TASK_GRAPH_ITEM_PAGE_MAX_ITEMS,
            });
        }
        if dependency_limit == 0 || dependency_limit > WORK_TASK_GRAPH_DEPENDENCY_PAGE_MAX_ITEMS {
            return Err(super::WorkDomainError::InvalidTaskGraphPageLimit {
                max_items: WORK_TASK_GRAPH_DEPENDENCY_PAGE_MAX_ITEMS,
            });
        }
        if (item_offset > 0 || dependency_offset > 0) && expected_graph_revision.is_none() {
            return Err(super::WorkDomainError::UnpinnedTaskGraphCursor);
        }
        Ok(Self {
            owner_id,
            work_id,
            branch_id,
            expected_graph_revision,
            item_offset,
            item_limit,
            dependency_offset,
            dependency_limit,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkTaskGraphCursor {
    pub graph_revision: GraphRevision,
    pub item_offset: u16,
    pub dependency_offset: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemExecutionStatus {
    NotStarted,
    Running,
    Waiting,
    Paused,
    Completed,
    Delegated,
    Failed,
    Cancelled,
}

impl WorkItemExecutionStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Delegated | Self::Failed | Self::Cancelled
        )
    }
}

/// Exact root-Run fact selected for one immutable WorkItem revision.
///
/// `graph_revision` records the graph cut at admission. A later graph may
/// still contain the same immutable item revision, so reconciliation does not
/// discard a valid attempt merely because unrelated declared work changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkItemExecutionRunRef {
    pub run_id: String,
    pub attempt_id: super::WorkItemAttemptId,
    pub graph_revision: GraphRevision,
    /// Atomic graph/intent cut published by the exact settlement that made the
    /// durable task graph complete. It is absent for non-final attempts and
    /// becomes stale when a later graph revision reopens Work.
    #[serde(skip)]
    pub(crate) terminal_cut: Option<WorkAttemptTerminalCut>,
    /// Internal execution role. Public task-board projections do not expose
    /// this authority fact.
    #[serde(skip)]
    pub execution_mode: WorkAttemptExecutionMode,
    pub run_generation: u64,
    pub last_event_idx: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkItemExecution {
    pub status: WorkItemExecutionStatus,
    pub terminal: bool,
    pub run: Option<WorkItemExecutionRunRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkItemDelivery {
    pub status: WorkItemDeliveryStatus,
    pub summary: Option<String>,
    pub blocker_kind: Option<super::WorkAttemptBlockerKind>,
    pub unavailable_capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemDeliveryStatus {
    Unreported,
    Delivered,
    Blocked,
    Failed,
}

impl WorkItemDelivery {
    pub(crate) fn unreported() -> Self {
        Self {
            status: WorkItemDeliveryStatus::Unreported,
            summary: None,
            blocker_kind: None,
            unavailable_capabilities: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkCheckFreshness {
    Current,
    CriteriaChanged,
    GraphChanged,
    SubjectUnavailable,
    SubjectChanged,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemVerificationStatus {
    Unknown,
    EvidenceAvailable,
    StaleEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkItemCheckFact {
    pub check_run_id: CheckRunId,
    pub criterion: CriterionRevisionRef,
    pub criterion_set_revision: CriterionSetRevision,
    pub graph_revision: GraphRevision,
    pub verifier_kind: CheckVerifierKind,
    pub outcome: CheckOutcome,
    pub coverage: CheckCoverage,
    pub subject_revision: WorkContentHash,
    pub evidence_ref_count: u16,
    pub produced_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub freshness: WorkCheckFreshness,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkItemVerification {
    pub status: WorkItemVerificationStatus,
    pub latest_check: Option<WorkItemCheckFact>,
}

impl WorkItemVerification {
    pub(crate) fn unknown() -> Self {
        Self {
            status: WorkItemVerificationStatus::Unknown,
            latest_check: None,
        }
    }

    pub(crate) fn from_check(check: WorkItemCheckFact) -> Self {
        let status = if check.freshness == WorkCheckFreshness::Current {
            WorkItemVerificationStatus::EvidenceAvailable
        } else {
            WorkItemVerificationStatus::StaleEvidence
        };
        Self {
            status,
            latest_check: Some(check),
        }
    }
}

impl WorkItemExecution {
    pub(crate) fn not_started() -> Self {
        Self {
            status: WorkItemExecutionStatus::NotStarted,
            terminal: false,
            run: None,
        }
    }

    pub(crate) fn from_run(status: WorkItemExecutionStatus, run: WorkItemExecutionRunRef) -> Self {
        debug_assert!(status != WorkItemExecutionStatus::NotStarted);
        Self {
            status,
            terminal: status.is_terminal(),
            run: Some(run),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkTaskGraphItem {
    pub item_id: WorkItemId,
    pub revision: WorkItemRevision,
    pub kind: WorkItemKind,
    pub objective: WorkItemText,
    pub expected_result: WorkItemText,
    pub declaration_state: WorkItemDeclarationState,
    pub execution: WorkItemExecution,
    pub delivery: WorkItemDelivery,
    pub verification: WorkItemVerification,
}

/// The server-internal execution projection used by the Work coordinator.
///
/// Unlike the paged task-board projection, this is an atomic, bounded view of
/// one Work graph (at most 256 items).  A coordinator needs the whole
/// dependency cut to select one task deterministically; making it stitch UI
/// pages would both add round trips and admit stale, mixed-revision choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkTaskExecutionSnapshot {
    basis: WorkPlanBasis,
    items: Vec<WorkTaskExecutionItem>,
    dependencies: Vec<WorkItemEdge>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkTaskExecutionItem {
    pub item_id: WorkItemId,
    pub revision: WorkItemRevision,
    pub kind: WorkItemKind,
    pub objective: WorkItemText,
    pub expected_result: WorkItemText,
    pub declaration_state: WorkItemDeclarationState,
    pub execution: WorkItemExecution,
    pub delivery: WorkItemDelivery,
}

/// A deterministic foreground-execution decision.  The model can author or
/// revise the graph, but it never gets to choose a concurrent worker, skip a
/// prerequisite, or treat a terminal run as a delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkTaskExecutionNext {
    Ready(WorkTaskExecutionItem),
    InFlight(WorkTaskExecutionItem),
    NeedsRecovery(WorkTaskExecutionItem),
    Blocked,
    Complete,
}

#[derive(Clone, Copy)]
enum DependencyReadiness {
    Visiting,
    Satisfied,
    Unsatisfied,
}

impl WorkTaskExecutionSnapshot {
    pub(crate) fn from_parts(
        basis: WorkPlanBasis,
        mut items: Vec<WorkPlanItem>,
        mut executions: BTreeMap<WorkItemRevisionRef, WorkItemExecution>,
        mut deliveries: BTreeMap<WorkItemRevisionRef, WorkItemDelivery>,
        dependencies: Vec<WorkItemEdge>,
    ) -> Result<Self, super::WorkRepositoryError> {
        items.sort_by(|left, right| left.item_id.cmp(&right.item_id));
        let items = items
            .into_iter()
            .map(|item| {
                let reference = item.revision_ref();
                WorkTaskExecutionItem {
                    item_id: item.item_id,
                    revision: item.revision,
                    kind: item.kind,
                    objective: item.objective,
                    expected_result: item.expected_result,
                    declaration_state: item.declaration_state,
                    execution: executions
                        .remove(&reference)
                        .unwrap_or_else(WorkItemExecution::not_started),
                    delivery: deliveries
                        .remove(&reference)
                        .unwrap_or_else(WorkItemDelivery::unreported),
                }
            })
            .collect();
        if !executions.is_empty() || !deliveries.is_empty() {
            return Err(super::WorkRepositoryError::corrupt(
                "Work task execution snapshot",
                std::io::Error::other(
                    "execution or delivery facts do not belong to the current Work graph",
                ),
            ));
        }
        Ok(Self {
            basis,
            items,
            dependencies,
        })
    }

    pub fn basis(&self) -> &WorkPlanBasis {
        &self.basis
    }

    pub fn items(&self) -> &[WorkTaskExecutionItem] {
        &self.items
    }

    pub fn dependencies(&self) -> &[WorkItemEdge] {
        &self.dependencies
    }

    /// Whether exactly one completed primary attempt from this durable Run
    /// produced the terminal graph cut currently owned by this Work branch.
    /// The exact owner generation that admitted the settlement must still own
    /// synthesis; neither a stale nor a future generation may borrow it.
    ///
    /// The cut is written only by the branch-locked settlement that changes
    /// the graph to `Complete`. Any later item revision or successor changes
    /// the current graph revision and therefore invalidates the old cut.
    pub fn final_synthesis_control_epoch(
        &self,
        run_id: &str,
        authorized_owner_generation: u64,
    ) -> Option<i64> {
        if !matches!(self.next_foreground_task(), WorkTaskExecutionNext::Complete) {
            return None;
        }
        let mut terminal_cut_items = self.items.iter().filter(|item| {
            item.execution.run.as_ref().is_some_and(|attempt| {
                attempt
                    .terminal_cut
                    .is_some_and(|cut| cut.graph_revision == self.basis.graph_revision)
            })
        });
        let item = terminal_cut_items.next()?;
        if terminal_cut_items.next().is_some() {
            return None;
        }
        let attempt = item.execution.run.as_ref()?;
        let eligible = item.kind == WorkItemKind::Task
            && item.declaration_state == WorkItemDeclarationState::Active
            && item.execution.status == WorkItemExecutionStatus::Completed
            && item.execution.terminal
            && item.delivery.status == WorkItemDeliveryStatus::Delivered
            && attempt.run_id == run_id
            && attempt.execution_mode == WorkAttemptExecutionMode::Primary
            && attempt.run_generation == authorized_owner_generation;
        eligible.then_some(attempt.terminal_cut?.control_epoch)
    }

    /// Select exactly one foreground task from durable state. Ordering is
    /// canonical item identity, and an unfinished prior attempt wins over
    /// starting unrelated work. Milestones are structural joins: their
    /// transitive task predecessors must be delivered before a downstream task
    /// can run, but a milestone never needs a synthetic execution attempt.
    pub fn next_foreground_task(&self) -> WorkTaskExecutionNext {
        let is_active_task = |item: &&WorkTaskExecutionItem| {
            item.kind == WorkItemKind::Task
                && item.declaration_state == WorkItemDeclarationState::Active
        };

        if let Some(item) = self.items.iter().filter(is_active_task).find(|item| {
            matches!(
                item.execution.status,
                WorkItemExecutionStatus::Running
                    | WorkItemExecutionStatus::Waiting
                    | WorkItemExecutionStatus::Paused
            )
        }) {
            return WorkTaskExecutionNext::InFlight(item.clone());
        }
        if let Some(item) = self.items.iter().filter(is_active_task).find(|item| {
            item.execution.status != WorkItemExecutionStatus::NotStarted
                && item.delivery.status != WorkItemDeliveryStatus::Delivered
        }) {
            return WorkTaskExecutionNext::NeedsRecovery(item.clone());
        }

        // Build the dependency cut once per selection. The previous helper
        // rebuilt both maps for every ready candidate, turning a bounded graph
        // into O(tasks * (items + edges)) work on every coordinator advance.
        let items_by_id = self
            .items
            .iter()
            .map(|item| (&item.item_id, item))
            .collect::<BTreeMap<_, _>>();
        let predecessors = self.dependencies.iter().fold(
            BTreeMap::<&WorkItemId, Vec<&WorkItemId>>::new(),
            |mut predecessors, edge| {
                predecessors
                    .entry(&edge.successor_item_id)
                    .or_default()
                    .push(&edge.predecessor_item_id);
                predecessors
            },
        );
        let mut readiness = BTreeMap::new();
        if let Some(item) = self.items.iter().filter(is_active_task).find(|item| {
            item.execution.status == WorkItemExecutionStatus::NotStarted
                && item.delivery.status == WorkItemDeliveryStatus::Unreported
                && Self::task_dependencies_are_delivered(
                    item,
                    &items_by_id,
                    &predecessors,
                    &mut readiness,
                )
        }) {
            return WorkTaskExecutionNext::Ready(item.clone());
        }
        if self
            .items
            .iter()
            .filter(is_active_task)
            .all(|item| item.delivery.status == WorkItemDeliveryStatus::Delivered)
        {
            WorkTaskExecutionNext::Complete
        } else {
            WorkTaskExecutionNext::Blocked
        }
    }

    fn task_dependencies_are_delivered<'a>(
        task: &'a WorkTaskExecutionItem,
        items_by_id: &BTreeMap<&'a WorkItemId, &'a WorkTaskExecutionItem>,
        predecessors: &BTreeMap<&'a WorkItemId, Vec<&'a WorkItemId>>,
        readiness: &mut BTreeMap<&'a WorkItemId, DependencyReadiness>,
    ) -> bool {
        predecessors
            .get(&task.item_id)
            .into_iter()
            .flatten()
            .all(|item_id| {
                Self::dependency_is_delivered(item_id, items_by_id, predecessors, readiness)
            })
    }

    fn dependency_is_delivered<'a>(
        item_id: &'a WorkItemId,
        items_by_id: &BTreeMap<&'a WorkItemId, &'a WorkTaskExecutionItem>,
        predecessors: &BTreeMap<&'a WorkItemId, Vec<&'a WorkItemId>>,
        readiness: &mut BTreeMap<&'a WorkItemId, DependencyReadiness>,
    ) -> bool {
        match readiness.get(item_id) {
            Some(DependencyReadiness::Satisfied) => return true,
            Some(DependencyReadiness::Unsatisfied | DependencyReadiness::Visiting) => return false,
            None => {}
        }
        readiness.insert(item_id, DependencyReadiness::Visiting);
        let satisfied = items_by_id.get(item_id).is_some_and(|item| {
            if item.declaration_state != WorkItemDeclarationState::Active {
                return false;
            }
            match item.kind {
                WorkItemKind::Task => item.delivery.status == WorkItemDeliveryStatus::Delivered,
                WorkItemKind::Milestone => {
                    predecessors
                        .get(item_id)
                        .into_iter()
                        .flatten()
                        .all(|predecessor_id| {
                            Self::dependency_is_delivered(
                                predecessor_id,
                                items_by_id,
                                predecessors,
                                readiness,
                            )
                        })
                }
            }
        });
        readiness.insert(
            item_id,
            if satisfied {
                DependencyReadiness::Satisfied
            } else {
                DependencyReadiness::Unsatisfied
            },
        );
        satisfied
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkTaskGraphItemPage {
    pub offset: u16,
    pub limit: u16,
    pub total: u16,
    pub entries: Vec<WorkTaskGraphItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkTaskGraphDependencyPage {
    pub offset: u16,
    pub limit: u16,
    pub total: u16,
    pub entries: Vec<WorkItemEdge>,
}

/// Public Task Graph slice over independent owners: immutable WorkItems,
/// durable root-Run facts, and exact Check facts. It never infers state from
/// transcript text. A terminal Run is not verification, and even a fresh
/// passed Check is only available evidence until required criteria aggregate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkTaskGraphPage {
    schema_version: u16,
    scope: super::ObservationScope,
    basis: WorkPlanBasis,
    cursor: WorkTaskGraphCursor,
    next_cursor: Option<WorkTaskGraphCursor>,
    items: WorkTaskGraphItemPage,
    dependencies: WorkTaskGraphDependencyPage,
}

impl WorkTaskGraphPage {
    pub(crate) fn from_parts(
        basis: WorkPlanBasis,
        query: &WorkTaskGraphQuery,
        items: Vec<WorkPlanItem>,
        mut executions: BTreeMap<WorkItemRevisionRef, WorkItemExecution>,
        mut deliveries: BTreeMap<WorkItemRevisionRef, WorkItemDelivery>,
        mut verifications: BTreeMap<WorkItemRevisionRef, WorkItemVerification>,
        dependencies: Vec<WorkItemEdge>,
    ) -> Result<Self, super::WorkRepositoryError> {
        let item_end = usize::from(query.item_offset) + items.len();
        let dependency_end = usize::from(query.dependency_offset) + dependencies.len();
        let item_total = basis.graph_item_count;
        let dependency_total = basis.graph_edge_count;
        if basis.work_id != query.work_id
            || basis.branch_id != query.branch_id
            || items.len() > usize::from(query.item_limit)
            || dependencies.len() > usize::from(query.dependency_limit)
            || item_end > usize::from(item_total)
            || dependency_end > usize::from(dependency_total)
        {
            return Err(super::WorkRepositoryError::corrupt(
                "Work Task Graph page",
                std::io::Error::other("page entries exceed their pinned graph bounds"),
            ));
        }
        let item_end = u16::try_from(item_end).map_err(|source| {
            super::WorkRepositoryError::corrupt("Work Task Graph item cursor", source)
        })?;
        let dependency_end = u16::try_from(dependency_end).map_err(|source| {
            super::WorkRepositoryError::corrupt("Work Task Graph dependency cursor", source)
        })?;
        let items = items
            .into_iter()
            .map(|item| {
                let execution = executions
                    .remove(&item.revision_ref())
                    .unwrap_or_else(WorkItemExecution::not_started);
                let verification = verifications
                    .remove(&item.revision_ref())
                    .unwrap_or_else(WorkItemVerification::unknown);
                let delivery = deliveries
                    .remove(&item.revision_ref())
                    .unwrap_or_else(WorkItemDelivery::unreported);
                WorkTaskGraphItem {
                    item_id: item.item_id,
                    revision: item.revision,
                    kind: item.kind,
                    objective: item.objective,
                    expected_result: item.expected_result,
                    declaration_state: item.declaration_state,
                    execution,
                    delivery,
                    verification,
                }
            })
            .collect();
        if !executions.is_empty() || !deliveries.is_empty() || !verifications.is_empty() {
            return Err(super::WorkRepositoryError::corrupt(
                "Work Task Graph fact projection",
                std::io::Error::other(
                    "execution, delivery, or verification facts do not belong to this bounded item page",
                ),
            ));
        }
        let cursor = WorkTaskGraphCursor {
            graph_revision: basis.graph_revision,
            item_offset: query.item_offset,
            dependency_offset: query.dependency_offset,
        };
        let next_cursor = (item_end < item_total || dependency_end < dependency_total).then_some(
            WorkTaskGraphCursor {
                graph_revision: basis.graph_revision,
                item_offset: item_end,
                dependency_offset: dependency_end,
            },
        );
        Ok(Self {
            schema_version: WORK_TASK_GRAPH_SCHEMA_VERSION,
            scope: super::ObservationScope::DeclaredWork,
            basis,
            cursor,
            next_cursor,
            items: WorkTaskGraphItemPage {
                offset: query.item_offset,
                limit: query.item_limit,
                total: item_total,
                entries: items,
            },
            dependencies: WorkTaskGraphDependencyPage {
                offset: query.dependency_offset,
                limit: query.dependency_limit,
                total: dependency_total,
                entries: dependencies,
            },
        })
    }

    pub fn basis(&self) -> &WorkPlanBasis {
        &self.basis
    }

    pub fn cursor(&self) -> &WorkTaskGraphCursor {
        &self.cursor
    }

    pub fn next_cursor(&self) -> Option<&WorkTaskGraphCursor> {
        self.next_cursor.as_ref()
    }

    pub fn items(&self) -> &WorkTaskGraphItemPage {
        &self.items
    }

    pub fn dependencies(&self) -> &WorkTaskGraphDependencyPage {
        &self.dependencies
    }
}

#[derive(Serialize)]
struct WorkPlanContextContent<'a> {
    schema_version: u16,
    basis: &'a WorkPlanBasis,
    items: &'a [WorkPlanItem],
    dependencies: &'a [WorkItemEdge],
}

/// Bounded, content-addressed planning input for one session-bound branch.
///
/// The context contains declared Work only. Run, invocation, and check state
/// remain separate authorities and are reconciled by later projections.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPlanContext {
    schema_version: u16,
    context_id: String,
    content_hash: WorkContentHash,
    basis: WorkPlanBasis,
    items: Vec<WorkPlanItem>,
    dependencies: Vec<WorkItemEdge>,
}

impl WorkPlanContext {
    pub(crate) fn from_parts(
        basis: WorkPlanBasis,
        mut items: Vec<WorkPlanItem>,
        dependencies: Vec<WorkItemEdge>,
    ) -> Result<Self, super::WorkRepositoryError> {
        items.sort_by(|left, right| left.item_id.cmp(&right.item_id));
        let graph_items = items
            .iter()
            .map(|item| WorkGraphItemChange::Existing(item.revision_ref()))
            .collect::<Vec<_>>();
        let graph = super::graph::validate_and_canonicalize_graph(&graph_items, &dependencies)
            .map_err(|source| super::WorkRepositoryError::corrupt("Work plan context", source))?;
        if graph
            .item_refs
            .iter()
            .zip(&items)
            .any(|(reference, item)| reference != &item.revision_ref())
            || graph.edges != dependencies
        {
            return Err(super::WorkRepositoryError::corrupt(
                "Work plan context",
                std::io::Error::other("planning facts are not canonical"),
            ));
        }
        if usize::from(basis.graph_item_count) != items.len()
            || usize::from(basis.graph_edge_count) != dependencies.len()
        {
            return Err(super::WorkRepositoryError::corrupt(
                "Work plan context",
                std::io::Error::other("planning facts do not match the immutable graph summary"),
            ));
        }
        let canonical = serde_json::to_vec(&WorkPlanContextContent {
            schema_version: WORK_PLAN_CONTEXT_SCHEMA_VERSION,
            basis: &basis,
            items: &items,
            dependencies: &dependencies,
        })
        .map_err(|source| super::WorkRepositoryError::ManifestEncoding {
            entity: "Work plan context",
            source,
        })?;
        let digest = format!("{:x}", Sha256::digest(canonical));
        Ok(Self {
            schema_version: WORK_PLAN_CONTEXT_SCHEMA_VERSION,
            context_id: format!("work-plan-context:{digest}"),
            content_hash: WorkContentHash::parse(format!("sha256:{digest}"))
                .expect("sha256 digest is a valid Work content hash"),
            basis,
            items,
            dependencies,
        })
    }

    pub fn context_id(&self) -> &str {
        &self.context_id
    }

    pub fn content_hash(&self) -> &WorkContentHash {
        &self.content_hash
    }

    pub fn basis(&self) -> &WorkPlanBasis {
        &self.basis
    }

    pub fn items(&self) -> &[WorkPlanItem] {
        &self.items
    }

    pub fn dependencies(&self) -> &[WorkItemEdge] {
        &self.dependencies
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::WorkItemEdgeKind;
    use chrono::TimeZone;

    fn hash(byte: char) -> WorkContentHash {
        WorkContentHash::parse(format!("sha256:{}", byte.to_string().repeat(64))).expect("hash")
    }

    fn basis(item_count: u16, edge_count: u16) -> WorkPlanBasis {
        WorkPlanBasis {
            work_id: WorkId::parse("work-1").expect("work"),
            work_revision: WorkRevision::INITIAL,
            goal_revision: GoalRevision::INITIAL,
            goal: WorkGoal::parse("Ship a bounded planning context.").expect("goal"),
            criteria_set_revision: CriterionSetRevision::INITIAL,
            criteria_member_count: 0,
            criteria_manifest_hash: hash('a'),
            branch_id: WorkBranchId::parse("branch-1").expect("branch"),
            branch_revision: WorkBranchRevision::INITIAL,
            branch_goal_revision: GoalRevision::INITIAL,
            branch_criteria_set_revision: CriterionSetRevision::INITIAL,
            branch_basis_graph_revision: GraphRevision::INITIAL,
            graph_revision: GraphRevision::INITIAL,
            graph_item_count: item_count,
            graph_edge_count: edge_count,
            graph_manifest_hash: hash('b'),
        }
    }

    fn item(id: &str) -> WorkPlanItem {
        item_with_kind(id, WorkItemKind::Task)
    }

    fn item_with_kind(id: &str, kind: WorkItemKind) -> WorkPlanItem {
        WorkPlanItem {
            item_id: WorkItemId::parse(id).expect("item"),
            revision: WorkItemRevision::INITIAL,
            kind,
            objective: WorkItemText::parse(format!("Implement {id}")).expect("objective"),
            expected_result: WorkItemText::parse(format!("Verify {id}")).expect("result"),
            declaration_state: WorkItemDeclarationState::Active,
        }
    }

    fn dependency(from: &str, to: &str) -> WorkItemEdge {
        WorkItemEdge {
            predecessor_item_id: WorkItemId::parse(from).expect("from"),
            successor_item_id: WorkItemId::parse(to).expect("to"),
            kind: WorkItemEdgeKind::Dependency,
        }
    }

    fn execution_snapshot(
        items: Vec<WorkPlanItem>,
        dependencies: Vec<WorkItemEdge>,
        executions: BTreeMap<WorkItemRevisionRef, WorkItemExecution>,
        deliveries: BTreeMap<WorkItemRevisionRef, WorkItemDelivery>,
    ) -> WorkTaskExecutionSnapshot {
        WorkTaskExecutionSnapshot::from_parts(
            basis(items.len() as u16, dependencies.len() as u16),
            items,
            executions,
            deliveries,
            dependencies,
        )
        .expect("coherent execution snapshot")
    }

    fn task_ref(id: &str) -> WorkItemRevisionRef {
        WorkItemRevisionRef {
            item_id: WorkItemId::parse(id).expect("item"),
            revision: WorkItemRevision::INITIAL,
        }
    }

    fn delivered() -> WorkItemDelivery {
        WorkItemDelivery {
            status: WorkItemDeliveryStatus::Delivered,
            summary: Some("delivered".into()),
            blocker_kind: None,
            unavailable_capabilities: Vec::new(),
        }
    }

    fn completed_execution(
        run_id: &str,
        generation: u64,
        terminal_graph_revision: Option<GraphRevision>,
    ) -> WorkItemExecution {
        WorkItemExecution::from_run(
            WorkItemExecutionStatus::Completed,
            WorkItemExecutionRunRef {
                run_id: run_id.to_string(),
                attempt_id: super::super::WorkItemAttemptId::parse("attempt-final")
                    .expect("attempt"),
                graph_revision: GraphRevision::INITIAL,
                terminal_cut: terminal_graph_revision
                    .and_then(|revision| WorkAttemptTerminalCut::new(revision, 5)),
                execution_mode: WorkAttemptExecutionMode::Primary,
                run_generation: generation,
                last_event_idx: 5,
                updated_at: Utc::now(),
            },
        )
    }

    #[test]
    fn final_synthesis_authority_is_exact_to_run_generation_and_terminal_graph_cut() {
        let terminal = execution_snapshot(
            vec![item("task-a")],
            Vec::new(),
            BTreeMap::from([(
                task_ref("task-a"),
                completed_execution("run-1", 7, Some(GraphRevision::INITIAL)),
            )]),
            BTreeMap::from([(task_ref("task-a"), delivered())]),
        );
        assert_eq!(terminal.final_synthesis_control_epoch("run-1", 7), Some(5));
        assert!(
            terminal.final_synthesis_control_epoch("run-1", 8).is_none(),
            "a newer generation may not borrow the prior owner's synthesis cut"
        );
        assert!(
            terminal.final_synthesis_control_epoch("run-1", 6).is_none(),
            "an attempt from a future owner generation must fail closed"
        );
        assert!(
            terminal
                .final_synthesis_control_epoch("run-other", 7)
                .is_none()
        );
        let public_run = serde_json::to_value(
            terminal.items()[0]
                .execution
                .run
                .as_ref()
                .expect("terminal run"),
        )
        .expect("serialize public run projection");
        assert!(
            public_run.get("terminal_graph_revision").is_none(),
            "internal synthesis authority must not change the Work wire projection"
        );
        assert!(public_run.get("terminal_control_epoch").is_none());
        assert!(public_run.get("terminal_cut").is_none());
        assert!(public_run.get("execution_mode").is_none());

        let no_terminal_cut = execution_snapshot(
            vec![item("task-a")],
            Vec::new(),
            BTreeMap::from([(task_ref("task-a"), completed_execution("run-1", 7, None))]),
            BTreeMap::from([(task_ref("task-a"), delivered())]),
        );
        assert!(
            no_terminal_cut
                .final_synthesis_control_epoch("run-1", 7)
                .is_none()
        );

        let mut revised = terminal.clone();
        revised.basis.graph_revision = GraphRevision::new(2).expect("revision");
        assert!(revised.final_synthesis_control_epoch("run-1", 7).is_none());

        let successor = execution_snapshot(
            vec![item("task-a"), item("task-b")],
            Vec::new(),
            BTreeMap::from([(
                task_ref("task-a"),
                completed_execution("run-1", 7, Some(GraphRevision::INITIAL)),
            )]),
            BTreeMap::from([(task_ref("task-a"), delivered())]),
        );
        assert!(
            successor
                .final_synthesis_control_epoch("run-1", 7)
                .is_none()
        );

        let duplicate_terminal_cut = execution_snapshot(
            vec![item("task-a"), item("task-b")],
            Vec::new(),
            BTreeMap::from([
                (
                    task_ref("task-a"),
                    completed_execution("run-1", 7, Some(GraphRevision::INITIAL)),
                ),
                (
                    task_ref("task-b"),
                    completed_execution("run-1", 7, Some(GraphRevision::INITIAL)),
                ),
            ]),
            BTreeMap::from([
                (task_ref("task-a"), delivered()),
                (task_ref("task-b"), delivered()),
            ]),
        );
        assert!(
            duplicate_terminal_cut
                .final_synthesis_control_epoch("run-1", 7)
                .is_none(),
            "two attempts claiming the same terminal cut must fail closed"
        );

        for corrupt_status in [
            WorkItemExecutionStatus::Failed,
            WorkItemExecutionStatus::Delegated,
        ] {
            let mut corrupt = terminal.clone();
            let run = corrupt.items[0]
                .execution
                .run
                .clone()
                .expect("terminal attempt");
            corrupt.items[0].execution = WorkItemExecution::from_run(corrupt_status, run);
            assert!(
                corrupt.final_synthesis_control_epoch("run-1", 7).is_none(),
                "only a completed terminal execution may authorize synthesis"
            );
        }

        let mut corrupt_delivery = terminal.clone();
        corrupt_delivery.items[0].delivery.status = WorkItemDeliveryStatus::Failed;
        assert!(
            corrupt_delivery
                .final_synthesis_control_epoch("run-1", 7)
                .is_none(),
            "a non-delivered terminal attempt must fail closed"
        );

        let mut delegated = terminal.clone();
        delegated.items[0]
            .execution
            .run
            .as_mut()
            .expect("terminal attempt")
            .execution_mode = WorkAttemptExecutionMode::Delegated;
        assert!(
            delegated
                .final_synthesis_control_epoch("run-1", 7)
                .is_none(),
            "a delegated execution must not authorize primary Work synthesis"
        );

        assert!(
            WorkAttemptTerminalCut::new(GraphRevision::INITIAL, -2).is_none(),
            "an invalid semantic epoch cannot enter the typed terminal fact"
        );
    }

    #[test]
    fn foreground_execution_selects_one_ready_task_in_canonical_dependency_order() {
        let items = vec![item("task-b"), item("task-a")];
        let dependencies = vec![dependency("task-a", "task-b")];
        let snapshot = execution_snapshot(
            items.clone(),
            dependencies.clone(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert!(matches!(
            snapshot.next_foreground_task(),
            WorkTaskExecutionNext::Ready(task) if task.item_id.as_str() == "task-a"
        ));

        let snapshot = execution_snapshot(
            items,
            dependencies,
            BTreeMap::new(),
            BTreeMap::from([(task_ref("task-a"), delivered())]),
        );
        assert!(matches!(
            snapshot.next_foreground_task(),
            WorkTaskExecutionNext::Ready(task) if task.item_id.as_str() == "task-b"
        ));
    }

    #[test]
    fn foreground_execution_projects_dependencies_through_structural_milestones() {
        let items = vec![
            item_with_kind("join", WorkItemKind::Milestone),
            item("investigate"),
            item("synthesize"),
        ];
        let dependencies = vec![
            dependency("investigate", "join"),
            dependency("join", "synthesize"),
        ];
        let snapshot = execution_snapshot(
            items.clone(),
            dependencies.clone(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert!(matches!(
            snapshot.next_foreground_task(),
            WorkTaskExecutionNext::Ready(task) if task.item_id.as_str() == "investigate"
        ));

        let snapshot = execution_snapshot(
            items,
            dependencies,
            BTreeMap::new(),
            BTreeMap::from([(task_ref("investigate"), delivered())]),
        );
        assert!(matches!(
            snapshot.next_foreground_task(),
            WorkTaskExecutionNext::Ready(task) if task.item_id.as_str() == "synthesize"
        ));
    }

    #[test]
    fn foreground_execution_accepts_shared_predecessor_at_structural_fan_in() {
        let items = vec![
            item("foundation"),
            item_with_kind("left", WorkItemKind::Milestone),
            item_with_kind("right", WorkItemKind::Milestone),
            item_with_kind("join", WorkItemKind::Milestone),
            item("integrate"),
        ];
        let dependencies = vec![
            dependency("foundation", "left"),
            dependency("foundation", "right"),
            dependency("left", "join"),
            dependency("right", "join"),
            dependency("join", "integrate"),
        ];
        let snapshot = execution_snapshot(
            items,
            dependencies,
            BTreeMap::new(),
            BTreeMap::from([(task_ref("foundation"), delivered())]),
        );

        assert!(matches!(
            snapshot.next_foreground_task(),
            WorkTaskExecutionNext::Ready(task) if task.item_id.as_str() == "integrate"
        ));
    }

    #[test]
    fn foreground_execution_never_skips_inflight_or_unsettled_task_attempts() {
        let items = vec![item("task-a"), item("task-b")];
        let in_flight = WorkItemExecution {
            status: WorkItemExecutionStatus::Running,
            terminal: false,
            run: None,
        };
        let snapshot = execution_snapshot(
            items.clone(),
            Vec::new(),
            BTreeMap::from([(task_ref("task-a"), in_flight)]),
            BTreeMap::new(),
        );
        assert!(matches!(
            snapshot.next_foreground_task(),
            WorkTaskExecutionNext::InFlight(task) if task.item_id.as_str() == "task-a"
        ));

        let unsettled = WorkItemExecution {
            status: WorkItemExecutionStatus::Completed,
            terminal: true,
            run: None,
        };
        let snapshot = execution_snapshot(
            items,
            Vec::new(),
            BTreeMap::from([(task_ref("task-a"), unsettled)]),
            BTreeMap::new(),
        );
        assert!(matches!(
            snapshot.next_foreground_task(),
            WorkTaskExecutionNext::NeedsRecovery(task) if task.item_id.as_str() == "task-a"
        ));
    }

    #[test]
    fn identity_is_canonical_and_independent_of_input_order() {
        let first = WorkPlanContext::from_parts(
            basis(2, 1),
            vec![item("task-b"), item("task-a")],
            vec![dependency("task-a", "task-b")],
        )
        .expect("context");
        let second = WorkPlanContext::from_parts(
            basis(2, 1),
            vec![item("task-a"), item("task-b")],
            vec![dependency("task-a", "task-b")],
        )
        .expect("context");
        assert_eq!(first, second);
        assert_eq!(first.items()[0].item_id.as_str(), "task-a");
    }

    #[test]
    fn incoherent_counts_and_cycles_are_classified_as_corruption() {
        assert!(matches!(
            WorkPlanContext::from_parts(basis(0, 0), vec![item("task")], vec![]),
            Err(super::super::WorkRepositoryError::Corrupt {
                entity: "Work plan context",
                ..
            })
        ));
        assert!(matches!(
            WorkPlanContext::from_parts(
                basis(2, 2),
                vec![item("task-a"), item("task-b")],
                vec![
                    dependency("task-a", "task-b"),
                    dependency("task-b", "task-a"),
                ],
            ),
            Err(super::super::WorkRepositoryError::Corrupt {
                entity: "Work plan context",
                ..
            })
        ));
    }

    #[test]
    fn task_graph_pages_are_bounded_revision_pinned_and_keep_run_identity_distinct() {
        let owner = super::super::WorkOwnerId::parse("owner-1").expect("owner");
        let query = WorkTaskGraphQuery::new(
            owner.clone(),
            WorkId::parse("work-1").expect("work"),
            WorkBranchId::parse("branch-1").expect("branch"),
            None,
            0,
            1,
            0,
            1,
        )
        .expect("first page");
        let page = WorkTaskGraphPage::from_parts(
            basis(2, 1),
            &query,
            vec![item("task-a")],
            BTreeMap::from([(
                WorkItemRevisionRef {
                    item_id: WorkItemId::parse("task-a").expect("item"),
                    revision: WorkItemRevision::INITIAL,
                },
                WorkItemExecution::from_run(
                    WorkItemExecutionStatus::Completed,
                    WorkItemExecutionRunRef {
                        run_id: "run-1".to_string(),
                        attempt_id: super::super::WorkItemAttemptId::parse("attempt-1")
                            .expect("attempt"),
                        graph_revision: GraphRevision::INITIAL,
                        terminal_cut: WorkAttemptTerminalCut::new(GraphRevision::INITIAL, 5),
                        execution_mode: WorkAttemptExecutionMode::Primary,
                        run_generation: 2,
                        last_event_idx: 5,
                        updated_at: Utc
                            .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
                            .single()
                            .expect("timestamp"),
                    },
                ),
            )]),
            BTreeMap::from([(
                WorkItemRevisionRef {
                    item_id: WorkItemId::parse("task-a").expect("item"),
                    revision: WorkItemRevision::INITIAL,
                },
                WorkItemDelivery {
                    status: WorkItemDeliveryStatus::Delivered,
                    summary: Some("migration applied".into()),
                    blocker_kind: None,
                    unavailable_capabilities: Vec::new(),
                },
            )]),
            BTreeMap::from([(
                WorkItemRevisionRef {
                    item_id: WorkItemId::parse("task-a").expect("item"),
                    revision: WorkItemRevision::INITIAL,
                },
                WorkItemVerification::from_check(WorkItemCheckFact {
                    check_run_id: CheckRunId::parse("check-1").expect("check"),
                    criterion: CriterionRevisionRef {
                        criterion_id: super::super::CriterionId::parse("criterion-1")
                            .expect("criterion"),
                        revision: super::super::CriterionRevision::INITIAL,
                    },
                    criterion_set_revision: CriterionSetRevision::INITIAL,
                    graph_revision: GraphRevision::INITIAL,
                    verifier_kind: CheckVerifierKind::Test,
                    outcome: CheckOutcome::Passed,
                    coverage: CheckCoverage::Complete,
                    subject_revision: hash('c'),
                    evidence_ref_count: 1,
                    produced_at: Utc
                        .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
                        .single()
                        .expect("timestamp"),
                    expires_at: None,
                    freshness: WorkCheckFreshness::Current,
                }),
            )]),
            vec![dependency("task-a", "task-b")],
        )
        .expect("page");
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../fixtures/contracts/work_task_graph_v2.json"
        ))
        .expect("shared Task Graph fixture");
        assert_eq!(
            serde_json::to_value(&page).expect("Rust Task Graph wire value"),
            expected,
            "Rust and TypeScript must consume the exact same Task Graph contract"
        );
        assert_eq!(page.items().entries.len(), 1);
        assert_eq!(
            page.items().entries[0].execution.status,
            WorkItemExecutionStatus::Completed
        );
        assert!(page.items().entries[0].execution.terminal);
        assert_eq!(
            page.items().entries[0].delivery.status,
            WorkItemDeliveryStatus::Delivered
        );
        assert_eq!(
            page.items().entries[0].verification.status,
            WorkItemVerificationStatus::EvidenceAvailable
        );
        assert_eq!(
            page.items().entries[0]
                .verification
                .latest_check
                .as_ref()
                .expect("check")
                .outcome,
            CheckOutcome::Passed
        );
        assert_eq!(page.dependencies().entries.len(), 1);
        assert_eq!(
            page.next_cursor(),
            Some(&WorkTaskGraphCursor {
                graph_revision: GraphRevision::INITIAL,
                item_offset: 1,
                dependency_offset: 1,
            })
        );

        let next = WorkTaskGraphQuery::new(
            owner,
            WorkId::parse("work-1").expect("work"),
            WorkBranchId::parse("branch-1").expect("branch"),
            Some(GraphRevision::INITIAL),
            1,
            1,
            1,
            1,
        )
        .expect("continuation");
        let terminal = WorkTaskGraphPage::from_parts(
            basis(2, 1),
            &next,
            vec![item("task-b")],
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            Vec::new(),
        )
        .expect("terminal page");
        assert!(terminal.next_cursor().is_none());
        assert_eq!(
            terminal.items().entries[0].execution,
            WorkItemExecution::not_started()
        );
        assert_eq!(
            terminal.items().entries[0].verification,
            WorkItemVerification::unknown()
        );
    }

    #[test]
    fn task_graph_continuation_cannot_be_unpinned_or_unbounded() {
        let query = |expected, item_offset, item_limit| {
            WorkTaskGraphQuery::new(
                super::super::WorkOwnerId::parse("owner-1").expect("owner"),
                WorkId::parse("work-1").expect("work"),
                WorkBranchId::parse("branch-1").expect("branch"),
                expected,
                item_offset,
                item_limit,
                0,
                1,
            )
        };
        assert!(matches!(
            query(None, 1, 1),
            Err(super::super::WorkDomainError::UnpinnedTaskGraphCursor)
        ));
        assert!(matches!(
            query(
                Some(GraphRevision::INITIAL),
                0,
                WORK_TASK_GRAPH_ITEM_PAGE_MAX_ITEMS + 1,
            ),
            Err(super::super::WorkDomainError::InvalidTaskGraphPageLimit { .. })
        ));
    }
}
