use super::{
    WorkBranchId, WorkBranchRevision, WorkChangeReason, WorkChangeRef, WorkDomainError, WorkId,
    WorkOwnerId, validate_resource_identity,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const WORK_ITEM_ID_MAX_CHARS: usize = 64;
const WORK_ITEM_ATTEMPT_ID_MAX_CHARS: usize = 64;
// Two text fields across the maximum 256-item admission stay below 4 MiB.
// This keeps one atomic bulk insert comfortably bounded for a shared service.
pub(crate) const WORK_ITEM_TEXT_MAX_BYTES: usize = 8 * 1024;
pub(crate) const WORK_GRAPH_MAX_ITEMS: usize = 256;
pub(crate) const WORK_GRAPH_MAX_EDGES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct WorkItemId(String);

impl WorkItemId {
    pub fn root() -> Self {
        Self("root".to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        validate_resource_identity("work_item_id", &value, WORK_ITEM_ID_MAX_CHARS)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkItemAttemptId(String);

impl WorkItemAttemptId {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        validate_resource_identity(
            "work_item_attempt_id",
            &value,
            WORK_ITEM_ATTEMPT_ID_MAX_CHARS,
        )?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkItemRevision(i64);

impl WorkItemRevision {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: i64) -> Result<Self, WorkDomainError> {
        if value < 1 {
            return Err(WorkDomainError::InvalidRevision {
                field: "work item",
                value,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    pub fn checked_next(self) -> Result<Self, WorkDomainError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(WorkDomainError::RevisionExhausted { field: "work item" })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemKind {
    Milestone,
    Task,
}

impl WorkItemKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Milestone => "milestone",
            Self::Task => "task",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "milestone" => Some(Self::Milestone),
            "task" => Some(Self::Task),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemDeclarationState {
    Active,
    Superseded,
    Cancelled,
}

impl WorkItemDeclarationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "superseded" => Some(Self::Superseded),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkItemText(String);

impl WorkItemText {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(WorkDomainError::InvalidWorkItemText {
                violation: super::WorkItemTextViolation::Empty,
            });
        }
        if value.len() > WORK_ITEM_TEXT_MAX_BYTES {
            return Err(WorkDomainError::InvalidWorkItemText {
                violation: super::WorkItemTextViolation::TooLarge {
                    max_bytes: WORK_ITEM_TEXT_MAX_BYTES,
                },
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct WorkItemRevisionRef {
    pub item_id: WorkItemId,
    pub revision: WorkItemRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NewWorkItem {
    pub item_id: WorkItemId,
    pub kind: WorkItemKind,
    pub objective: WorkItemText,
    pub expected_result: WorkItemText,
}

/// One immutable replacement revision for an existing Work item. The current
/// graph must contain `expected_revision`; applying the graph change creates
/// exactly its successor and never edits the previous declaration in place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkItemRevisionChange {
    pub item_id: WorkItemId,
    pub expected_revision: WorkItemRevision,
    pub kind: WorkItemKind,
    pub objective: WorkItemText,
    pub expected_result: WorkItemText,
    pub declaration_state: WorkItemDeclarationState,
    /// Assigned under the WorkItem identity row lock. It is deliberately not
    /// part of the proposal payload because sibling branches may allocate
    /// different successors from the same parent revision.
    #[serde(skip)]
    pub(crate) result_revision: Option<WorkItemRevision>,
}

impl WorkItemRevisionChange {
    pub fn new(
        item_id: WorkItemId,
        expected_revision: WorkItemRevision,
        kind: WorkItemKind,
        objective: WorkItemText,
        expected_result: WorkItemText,
        declaration_state: WorkItemDeclarationState,
    ) -> Self {
        Self {
            item_id,
            expected_revision,
            kind,
            objective,
            expected_result,
            declaration_state,
            result_revision: None,
        }
    }

    pub fn result_ref(&self) -> Result<WorkItemRevisionRef, WorkDomainError> {
        Ok(WorkItemRevisionRef {
            item_id: self.item_id.clone(),
            revision: self.result_revision.ok_or_else(|| {
                WorkDomainError::UnallocatedWorkItemRevision {
                    item_id: self.item_id.as_str().to_string(),
                }
            })?,
        })
    }

    pub fn expected_ref(&self) -> WorkItemRevisionRef {
        WorkItemRevisionRef {
            item_id: self.item_id.clone(),
            revision: self.expected_revision,
        }
    }

    pub(crate) fn assign_result_revision(&mut self, revision: WorkItemRevision) {
        self.result_revision = Some(revision);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkGraphItemChange {
    Existing(WorkItemRevisionRef),
    New(NewWorkItem),
    Revised(WorkItemRevisionChange),
}

impl WorkGraphItemChange {
    pub(crate) fn revision_ref(&self) -> Result<WorkItemRevisionRef, WorkDomainError> {
        match self {
            Self::Existing(reference) => Ok(reference.clone()),
            Self::New(item) => Ok(WorkItemRevisionRef {
                item_id: item.item_id.clone(),
                revision: WorkItemRevision::INITIAL,
            }),
            Self::Revised(item) => item.result_ref(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemEdgeKind {
    Dependency,
}

impl WorkItemEdgeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dependency => "dependency",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct WorkItemEdge {
    pub predecessor_item_id: WorkItemId,
    pub successor_item_id: WorkItemId,
    pub kind: WorkItemEdgeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkGraphChange {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_graph_revision: super::GraphRevision,
    pub items: Vec<WorkGraphItemChange>,
    pub edges: Vec<WorkItemEdge>,
    pub source_ref: WorkChangeRef,
    pub reason: Option<WorkChangeReason>,
}

pub(crate) struct CanonicalGraph {
    pub item_refs: Vec<WorkItemRevisionRef>,
    pub edges: Vec<WorkItemEdge>,
}

pub(crate) fn validate_and_canonicalize_graph(
    items: &[WorkGraphItemChange],
    edges: &[WorkItemEdge],
) -> Result<CanonicalGraph, WorkDomainError> {
    if items.len() > WORK_GRAPH_MAX_ITEMS {
        return Err(WorkDomainError::TooManyWorkItems {
            max_items: WORK_GRAPH_MAX_ITEMS,
        });
    }
    if edges.len() > WORK_GRAPH_MAX_EDGES {
        return Err(WorkDomainError::TooManyWorkItemEdges {
            max_edges: WORK_GRAPH_MAX_EDGES,
        });
    }

    let mut item_refs = items
        .iter()
        .map(WorkGraphItemChange::revision_ref)
        .collect::<Result<Vec<_>, _>>()?;
    item_refs.sort();
    if let Some(duplicate) = item_refs
        .windows(2)
        .find(|pair| pair[0].item_id == pair[1].item_id)
    {
        return Err(WorkDomainError::DuplicateWorkItem {
            item_id: duplicate[0].item_id.as_str().to_string(),
        });
    }
    let item_ids = item_refs
        .iter()
        .map(|reference| reference.item_id.clone())
        .collect::<BTreeSet<_>>();

    let mut canonical_edges = edges.to_vec();
    canonical_edges.sort();
    if canonical_edges.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WorkDomainError::DuplicateWorkItemEdge);
    }
    let mut indegree = item_ids
        .iter()
        .cloned()
        .map(|item_id| (item_id, 0usize))
        .collect::<BTreeMap<_, _>>();
    let mut successors = BTreeMap::<WorkItemId, Vec<WorkItemId>>::new();
    for edge in &canonical_edges {
        if edge.predecessor_item_id == edge.successor_item_id {
            return Err(WorkDomainError::SelfDependentWorkItem {
                item_id: edge.predecessor_item_id.as_str().to_string(),
            });
        }
        for endpoint in [&edge.predecessor_item_id, &edge.successor_item_id] {
            if !item_ids.contains(endpoint) {
                return Err(WorkDomainError::UnknownWorkItemEdgeEndpoint {
                    item_id: endpoint.as_str().to_string(),
                });
            }
        }
        *indegree
            .get_mut(&edge.successor_item_id)
            .expect("validated graph endpoint") += 1;
        successors
            .entry(edge.predecessor_item_id.clone())
            .or_default()
            .push(edge.successor_item_id.clone());
    }

    let mut ready = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(item_id, _)| item_id.clone())
        .collect::<VecDeque<_>>();
    let mut visited = 0usize;
    while let Some(item_id) = ready.pop_front() {
        visited += 1;
        for successor in successors.get(&item_id).into_iter().flatten() {
            let degree = indegree
                .get_mut(successor)
                .expect("validated graph successor");
            *degree -= 1;
            if *degree == 0 {
                ready.push_back(successor.clone());
            }
        }
    }
    if visited != item_ids.len() {
        return Err(WorkDomainError::CyclicWorkItemGraph);
    }

    Ok(CanonicalGraph {
        item_refs,
        edges: canonical_edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> WorkGraphItemChange {
        WorkGraphItemChange::New(NewWorkItem {
            item_id: WorkItemId::parse(id).expect("id"),
            kind: WorkItemKind::Task,
            objective: WorkItemText::parse(format!("Implement {id}")).expect("objective"),
            expected_result: WorkItemText::parse(format!("{id} is proven")).expect("result"),
        })
    }

    fn edge(from: &str, to: &str) -> WorkItemEdge {
        WorkItemEdge {
            predecessor_item_id: WorkItemId::parse(from).expect("from"),
            successor_item_id: WorkItemId::parse(to).expect("to"),
            kind: WorkItemEdgeKind::Dependency,
        }
    }

    #[test]
    fn graph_is_canonical_and_dependency_direction_is_unambiguous() {
        let graph = validate_and_canonicalize_graph(
            &[item("task-b"), item("task-a")],
            &[edge("task-a", "task-b")],
        )
        .expect("graph");
        assert_eq!(graph.item_refs[0].item_id.as_str(), "task-a");
        assert_eq!(graph.edges[0].predecessor_item_id.as_str(), "task-a");
        assert_eq!(graph.edges[0].successor_item_id.as_str(), "task-b");
    }

    #[test]
    fn invalid_edges_and_cycles_fail_before_persistence() {
        assert!(matches!(
            validate_and_canonicalize_graph(&[item("task-a")], &[edge("task-a", "missing")]),
            Err(WorkDomainError::UnknownWorkItemEdgeEndpoint { .. })
        ));
        assert!(matches!(
            validate_and_canonicalize_graph(
                &[item("task-a"), item("task-b")],
                &[edge("task-a", "task-b"), edge("task-b", "task-a")],
            ),
            Err(WorkDomainError::CyclicWorkItemGraph)
        ));
    }

    #[test]
    fn empty_graph_is_explicitly_valid() {
        let graph = validate_and_canonicalize_graph(&[], &[]).expect("empty graph");
        assert!(graph.item_refs.is_empty());
        assert!(graph.edges.is_empty());
    }

    #[test]
    fn item_text_is_bounded_by_the_atomic_graph_admission_budget() {
        assert!(WorkItemText::parse("x".repeat(WORK_ITEM_TEXT_MAX_BYTES)).is_ok());
        assert!(matches!(
            WorkItemText::parse("x".repeat(WORK_ITEM_TEXT_MAX_BYTES + 1)),
            Err(WorkDomainError::InvalidWorkItemText {
                violation: super::super::WorkItemTextViolation::TooLarge { .. }
            })
        ));
    }

    #[test]
    fn revised_items_use_the_repository_allocated_revision_and_cannot_overflow() {
        let mut revision = WorkItemRevisionChange::new(
            WorkItemId::parse("task-a").expect("id"),
            WorkItemRevision::INITIAL,
            WorkItemKind::Task,
            WorkItemText::parse("Revised objective").expect("objective"),
            WorkItemText::parse("Revised result").expect("result"),
            WorkItemDeclarationState::Active,
        );
        revision.assign_result_revision(WorkItemRevision::new(2).expect("revision"));
        let revised = WorkGraphItemChange::Revised(revision);
        let graph = validate_and_canonicalize_graph(&[revised], &[]).expect("revised graph");
        assert_eq!(graph.item_refs[0].revision.get(), 2);

        assert!(matches!(
            WorkItemRevision(i64::MAX).checked_next(),
            Err(WorkDomainError::RevisionExhausted { field: "work item" })
        ));
    }
}
