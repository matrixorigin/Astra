use super::graph::{CanonicalGraph, validate_and_canonicalize_graph};
use super::repository::{DatabaseWorkRepository, WorkConflictResource, WorkRepositoryError};
use super::{
    ForkCursorRef, GraphRevision, WorkBranchRecord, WorkBranchRecordParts, WorkBranchRevision,
    WorkGraphChange, WorkGraphItemChange, WorkItemDeclarationState, WorkItemId, WorkItemKind,
    WorkItemRevision, WorkItemRevisionRef, WorkItemText,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, Transaction, query};
use std::collections::BTreeSet;

const GRAPH_MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct GraphManifestV1<'a> {
    schema_version: u32,
    item_revisions: &'a [WorkItemRevisionRef],
    edges: &'a [super::WorkItemEdge],
}

#[derive(Serialize)]
#[serde(tag = "change_kind", rename_all = "snake_case")]
enum GraphReplacementItemV1<'a> {
    Existing {
        item_id: &'a WorkItemId,
        revision: WorkItemRevision,
    },
    New {
        item_id: &'a WorkItemId,
        revision: WorkItemRevision,
        kind: WorkItemKind,
        objective: &'a WorkItemText,
        expected_result: &'a WorkItemText,
    },
    Revised {
        item_id: &'a WorkItemId,
        expected_revision: WorkItemRevision,
        revision: WorkItemRevision,
        kind: WorkItemKind,
        objective: &'a WorkItemText,
        expected_result: &'a WorkItemText,
        declaration_state: WorkItemDeclarationState,
    },
}

impl GraphReplacementItemV1<'_> {
    fn item_id(&self) -> &WorkItemId {
        match self {
            Self::Existing { item_id, .. }
            | Self::New { item_id, .. }
            | Self::Revised { item_id, .. } => item_id,
        }
    }
}

#[derive(Serialize)]
struct GraphReplacementV1<'a> {
    schema_version: u32,
    items: Vec<GraphReplacementItemV1<'a>>,
    edges: &'a [super::WorkItemEdge],
}

struct EncodedGraph {
    items_json: String,
    edges_json: String,
    manifest_hash: String,
    replacement_hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedItemRevisionRef {
    item_id: String,
    revision: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedItemEdge {
    predecessor_item_id: String,
    successor_item_id: String,
    kind: String,
}

pub(super) struct PersistedGraph {
    pub(super) item_refs: Vec<WorkItemRevisionRef>,
    pub(super) edges: Vec<super::WorkItemEdge>,
}

pub(super) fn decode_persisted_graph(
    item_manifest_json: &str,
    item_count: i32,
    edge_manifest_json: &str,
    edge_count: i32,
) -> Result<PersistedGraph, WorkRepositoryError> {
    let item_wires: Vec<PersistedItemRevisionRef> = serde_json::from_str(item_manifest_json)
        .map_err(|source| WorkRepositoryError::corrupt("Work graph item manifest", source))?;
    let edge_wires: Vec<PersistedItemEdge> = serde_json::from_str(edge_manifest_json)
        .map_err(|source| WorkRepositoryError::corrupt("Work graph edge manifest", source))?;
    if i32::try_from(item_wires.len()).ok() != Some(item_count)
        || i32::try_from(edge_wires.len()).ok() != Some(edge_count)
    {
        return Err(WorkRepositoryError::corrupt(
            "Work graph manifest",
            std::io::Error::other("manifest count does not match its immutable summary"),
        ));
    }
    let item_refs = item_wires
        .into_iter()
        .map(|item| {
            Ok(WorkItemRevisionRef {
                item_id: WorkItemId::parse(item.item_id).map_err(|source| {
                    WorkRepositoryError::corrupt("Work graph item manifest", source)
                })?,
                revision: WorkItemRevision::new(item.revision).map_err(|source| {
                    WorkRepositoryError::corrupt("Work graph item manifest", source)
                })?,
            })
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    if item_refs
        .windows(2)
        .any(|pair| pair[0].item_id >= pair[1].item_id)
    {
        return Err(WorkRepositoryError::corrupt(
            "Work graph item manifest",
            std::io::Error::other("item identities are not canonical and unique"),
        ));
    }
    let mut edges = edge_wires
        .into_iter()
        .map(|edge| {
            if edge.kind != "dependency" {
                return Err(WorkRepositoryError::corrupt(
                    "Work graph edge manifest",
                    std::io::Error::other("unknown edge kind"),
                ));
            }
            Ok(super::WorkItemEdge {
                predecessor_item_id: WorkItemId::parse(edge.predecessor_item_id).map_err(
                    |source| WorkRepositoryError::corrupt("Work graph edge manifest", source),
                )?,
                successor_item_id: WorkItemId::parse(edge.successor_item_id).map_err(|source| {
                    WorkRepositoryError::corrupt("Work graph edge manifest", source)
                })?,
                kind: super::WorkItemEdgeKind::Dependency,
            })
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    let original_edges = edges.clone();
    edges.sort_unstable();
    if edges != original_edges || edges.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WorkRepositoryError::corrupt(
            "Work graph edge manifest",
            std::io::Error::other("edges are not canonical and unique"),
        ));
    }
    Ok(PersistedGraph { item_refs, edges })
}

pub(super) fn validate_persisted_graph_hash(
    graph: &PersistedGraph,
    persisted_hash: &str,
) -> Result<(), WorkRepositoryError> {
    let manifest = serde_json::to_string(&GraphManifestV1 {
        schema_version: GRAPH_MANIFEST_SCHEMA_VERSION,
        item_revisions: &graph.item_refs,
        edges: &graph.edges,
    })
    .map_err(|source| WorkRepositoryError::ManifestEncoding {
        entity: "Work graph manifest",
        source,
    })?;
    let actual_hash = format!("sha256:{:x}", Sha256::digest(manifest.as_bytes()));
    if actual_hash != persisted_hash {
        return Err(WorkRepositoryError::corrupt(
            "Work graph manifest",
            std::io::Error::other("manifest content does not match its immutable hash"),
        ));
    }
    Ok(())
}

fn encode_graph(
    graph: &CanonicalGraph,
    change: &WorkGraphChange,
) -> Result<EncodedGraph, WorkRepositoryError> {
    let items_json = serde_json::to_string(&graph.item_refs).map_err(|source| {
        WorkRepositoryError::ManifestEncoding {
            entity: "WorkItem revision manifest",
            source,
        }
    })?;
    let edges_json = serde_json::to_string(&graph.edges).map_err(|source| {
        WorkRepositoryError::ManifestEncoding {
            entity: "WorkItem edge manifest",
            source,
        }
    })?;
    let manifest = serde_json::to_string(&GraphManifestV1 {
        schema_version: GRAPH_MANIFEST_SCHEMA_VERSION,
        item_revisions: &graph.item_refs,
        edges: &graph.edges,
    })
    .map_err(|source| WorkRepositoryError::ManifestEncoding {
        entity: "Work graph manifest",
        source,
    })?;
    let mut replacement_items = change
        .items
        .iter()
        .map(|item| match item {
            WorkGraphItemChange::Existing(reference) => GraphReplacementItemV1::Existing {
                item_id: &reference.item_id,
                revision: reference.revision,
            },
            WorkGraphItemChange::New(item) => GraphReplacementItemV1::New {
                item_id: &item.item_id,
                revision: WorkItemRevision::INITIAL,
                kind: item.kind,
                objective: &item.objective,
                expected_result: &item.expected_result,
            },
            WorkGraphItemChange::Revised(item) => GraphReplacementItemV1::Revised {
                item_id: &item.item_id,
                expected_revision: item.expected_revision,
                revision: item
                    .result_revision
                    .expect("repository allocates revised item revisions"),
                kind: item.kind,
                objective: &item.objective,
                expected_result: &item.expected_result,
                declaration_state: item.declaration_state,
            },
        })
        .collect::<Vec<_>>();
    replacement_items.sort_by(|left, right| left.item_id().cmp(right.item_id()));
    let replacement = serde_json::to_string(&GraphReplacementV1 {
        schema_version: GRAPH_MANIFEST_SCHEMA_VERSION,
        items: replacement_items,
        edges: &graph.edges,
    })
    .map_err(|source| WorkRepositoryError::ManifestEncoding {
        entity: "Work graph replacement",
        source,
    })?;
    Ok(EncodedGraph {
        items_json,
        edges_json,
        manifest_hash: format!("sha256:{:x}", Sha256::digest(manifest.as_bytes())),
        replacement_hash: format!("sha256:{:x}", Sha256::digest(replacement.as_bytes())),
    })
}

async fn verify_existing_item_revisions(
    transaction: &mut Transaction<'_, MySql>,
    change: &WorkGraphChange,
) -> Result<(), WorkRepositoryError> {
    let expected = change
        .items
        .iter()
        .filter_map(|item| match item {
            WorkGraphItemChange::Existing(reference) => Some(reference.clone()),
            WorkGraphItemChange::New(_) => None,
            WorkGraphItemChange::Revised(item) => Some(item.expected_ref()),
        })
        .collect::<Vec<_>>();
    if expected.is_empty() {
        return Ok(());
    }
    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT item_id, revision FROM work_item_revisions WHERE owner_id = ",
    );
    builder
        .push_bind(change.owner_id.as_str())
        .push(" AND work_id = ")
        .push_bind(change.work_id.as_str())
        .push(" AND (");
    for (index, reference) in expected.iter().enumerate() {
        if index > 0 {
            builder.push(" OR ");
        }
        builder
            .push("(item_id = ")
            .push_bind(reference.item_id.as_str())
            .push(" AND revision = ")
            .push_bind(reference.revision.get())
            .push(")");
    }
    builder.push(")");
    let found = builder
        .build()
        .fetch_all(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("validate WorkItem revision references", source)
        })?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get::<String, _>("item_id")
                    .map_err(|source| WorkRepositoryError::corrupt("WorkItem revision", source))?,
                row.try_get::<i64, _>("revision")
                    .map_err(|source| WorkRepositoryError::corrupt("WorkItem revision", source))?,
            ))
        })
        .collect::<Result<BTreeSet<_>, WorkRepositoryError>>()?;
    let missing = expected
        .into_iter()
        .filter(|reference| {
            !found.contains(&(
                reference.item_id.as_str().to_string(),
                reference.revision.get(),
            ))
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(WorkRepositoryError::MissingWorkItemRevisions { missing })
    }
}

async fn verify_revised_item_bases(
    transaction: &mut Transaction<'_, MySql>,
    change: &WorkGraphChange,
) -> Result<(), WorkRepositoryError> {
    let expected = change
        .items
        .iter()
        .filter_map(|item| match item {
            WorkGraphItemChange::Revised(item) => Some(item.expected_ref()),
            WorkGraphItemChange::Existing(_) | WorkGraphItemChange::New(_) => None,
        })
        .collect::<BTreeSet<_>>();
    if expected.is_empty() {
        return Ok(());
    }
    let row = query(
        "SELECT item_revision_manifest_json, item_count, edge_manifest_json, edge_count
         FROM work_graph_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = ? LIMIT 1",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(change.expected_graph_revision.get())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load revised item graph basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let graph = decode_persisted_graph(
        &row.try_get::<String, _>("item_revision_manifest_json")
            .map_err(|source| WorkRepositoryError::corrupt("revised item graph basis", source))?,
        row.try_get("item_count")
            .map_err(|source| WorkRepositoryError::corrupt("revised item graph basis", source))?,
        &row.try_get::<String, _>("edge_manifest_json")
            .map_err(|source| WorkRepositoryError::corrupt("revised item graph basis", source))?,
        row.try_get("edge_count")
            .map_err(|source| WorkRepositoryError::corrupt("revised item graph basis", source))?,
    )?;
    let current = graph.item_refs.into_iter().collect::<BTreeSet<_>>();
    let missing = expected.difference(&current).cloned().collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(WorkRepositoryError::MissingWorkItemRevisions { missing })
    }
}

async fn allocate_revised_item_revisions(
    transaction: &mut Transaction<'_, MySql>,
    change: &mut WorkGraphChange,
) -> Result<(), WorkRepositoryError> {
    let mut revised_indices = change
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            WorkGraphItemChange::Revised(item) => Some((item.item_id.clone(), index)),
            WorkGraphItemChange::Existing(_) | WorkGraphItemChange::New(_) => None,
        })
        .collect::<Vec<_>>();
    revised_indices.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for (item_id, index) in revised_indices {
        let updated = query(
            "UPDATE work_items SET last_revision = last_revision + 1
             WHERE owner_id = ? AND work_id = ? AND item_id = ?
               AND last_revision < 9223372036854775807",
        )
        .bind(change.owner_id.as_str())
        .bind(change.work_id.as_str())
        .bind(item_id.as_str())
        .execute(&mut **transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("allocate WorkItem revision", source))?;
        if updated.rows_affected() != 1 {
            let last_revision = query(
                "SELECT last_revision FROM work_items
                 WHERE owner_id = ? AND work_id = ? AND item_id = ? LIMIT 1",
            )
            .bind(change.owner_id.as_str())
            .bind(change.work_id.as_str())
            .bind(item_id.as_str())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("read WorkItem revision sequence", source)
            })?;
            return match last_revision {
                None => Err(WorkRepositoryError::NotFound),
                Some(row)
                    if row.try_get::<i64, _>("last_revision").map_err(|source| {
                        WorkRepositoryError::corrupt("WorkItem revision sequence", source)
                    })? == i64::MAX =>
                {
                    Err(super::repository::invalid_mutation(
                        super::WorkDomainError::RevisionExhausted { field: "work item" },
                    ))
                }
                Some(_) => Err(WorkRepositoryError::corrupt(
                    "WorkItem revision sequence",
                    std::io::Error::other("revision allocation updated an unexpected row count"),
                )),
            };
        }
        let allocated: i64 = query(
            "SELECT last_revision FROM work_items
             WHERE owner_id = ? AND work_id = ? AND item_id = ? LIMIT 1",
        )
        .bind(change.owner_id.as_str())
        .bind(change.work_id.as_str())
        .bind(item_id.as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("read allocated WorkItem revision", source)
        })?
        .try_get("last_revision")
        .map_err(|source| WorkRepositoryError::corrupt("WorkItem revision sequence", source))?;
        let revision = WorkItemRevision::new(allocated)
            .map_err(|source| WorkRepositoryError::corrupt("WorkItem revision sequence", source))?;
        let WorkGraphItemChange::Revised(item) = &mut change.items[index] else {
            unreachable!("revised item index remains stable while allocating")
        };
        item.assign_result_revision(revision);
    }
    Ok(())
}

async fn insert_new_items(
    transaction: &mut Transaction<'_, MySql>,
    change: &WorkGraphChange,
) -> Result<(), WorkRepositoryError> {
    let items = change
        .items
        .iter()
        .filter_map(|item| match item {
            WorkGraphItemChange::Existing(_) | WorkGraphItemChange::Revised(_) => None,
            WorkGraphItemChange::New(item) => Some(item),
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        return Ok(());
    }
    let mut identities = QueryBuilder::<MySql>::new(
        "INSERT INTO work_items (owner_id, work_id, item_id, last_revision) ",
    );
    identities.push_values(&items, |mut row, item| {
        row.push_bind(change.owner_id.as_str())
            .push_bind(change.work_id.as_str())
            .push_bind(item.item_id.as_str())
            .push_bind(WorkItemRevision::INITIAL.get());
    });
    identities
        .build()
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "insert WorkItem identities",
                WorkConflictResource::WorkItemIdentity,
                source,
            )
        })?;

    let mut revisions = QueryBuilder::<MySql>::new(
        "INSERT INTO work_item_revisions
         (owner_id, work_id, item_id, revision, parent_revision, item_kind, objective,
          expected_result, declaration_state, source_ref) ",
    );
    revisions.push_values(&items, |mut row, item| {
        row.push_bind(change.owner_id.as_str())
            .push_bind(change.work_id.as_str())
            .push_bind(item.item_id.as_str())
            .push_bind(WorkItemRevision::INITIAL.get())
            .push_bind(Option::<i64>::None)
            .push_bind(item.kind.as_str())
            .push_bind(item.objective.as_str())
            .push_bind(item.expected_result.as_str())
            .push_bind("active")
            .push_bind(change.source_ref.as_str());
    });
    revisions
        .build()
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "insert WorkItem revisions",
                WorkConflictResource::WorkItemRevision,
                source,
            )
        })?;
    Ok(())
}

async fn insert_revised_items(
    transaction: &mut Transaction<'_, MySql>,
    change: &WorkGraphChange,
) -> Result<(), WorkRepositoryError> {
    let items = change
        .items
        .iter()
        .filter_map(|item| match item {
            WorkGraphItemChange::Revised(item) => Some(item),
            WorkGraphItemChange::Existing(_) | WorkGraphItemChange::New(_) => None,
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        return Ok(());
    }
    let mut revisions = QueryBuilder::<MySql>::new(
        "INSERT INTO work_item_revisions
         (owner_id, work_id, item_id, revision, parent_revision, item_kind, objective,
          expected_result, declaration_state, source_ref) ",
    );
    revisions.push_values(&items, |mut row, item| {
        row.push_bind(change.owner_id.as_str())
            .push_bind(change.work_id.as_str())
            .push_bind(item.item_id.as_str())
            .push_bind(
                item.result_revision
                    .expect("repository allocates revised item revisions")
                    .get(),
            )
            .push_bind(item.expected_revision.get())
            .push_bind(item.kind.as_str())
            .push_bind(item.objective.as_str())
            .push_bind(item.expected_result.as_str())
            .push_bind(item.declaration_state.as_str())
            .push_bind(change.source_ref.as_str());
    });
    revisions
        .build()
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "insert revised WorkItem revisions",
                WorkConflictResource::WorkItemRevision,
                source,
            )
        })?;
    Ok(())
}

async fn insert_edges(
    transaction: &mut Transaction<'_, MySql>,
    change: &WorkGraphChange,
    graph: &CanonicalGraph,
    graph_revision: GraphRevision,
) -> Result<(), WorkRepositoryError> {
    if graph.edges.is_empty() {
        return Ok(());
    }
    let mut edges = QueryBuilder::<MySql>::new(
        "INSERT INTO work_item_edges
         (owner_id, work_id, graph_revision, predecessor_item_id,
          successor_item_id, edge_kind) ",
    );
    edges.push_values(&graph.edges, |mut row, edge| {
        row.push_bind(change.owner_id.as_str())
            .push_bind(change.work_id.as_str())
            .push_bind(graph_revision.get())
            .push_bind(edge.predecessor_item_id.as_str())
            .push_bind(edge.successor_item_id.as_str())
            .push_bind(edge.kind.as_str());
    });
    edges
        .build()
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "insert WorkItem edges",
                WorkConflictResource::WorkItemEdge,
                source,
            )
        })?;
    Ok(())
}

fn decode_timestamp(value: String) -> Result<DateTime<Utc>, WorkRepositoryError> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))
}

pub(super) async fn load_branch_by_identity(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &super::WorkOwnerId,
    work_id: &super::WorkId,
    branch_id: &super::WorkBranchId,
) -> Result<WorkBranchRecord, WorkRepositoryError> {
    let row = query(
        "SELECT work_id, branch_id, branch_revision, session_id, origin_branch_id, fork_cursor,
                goal_revision_ref, criteria_set_revision_ref, basis_graph_revision,
                current_graph_revision,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                DATE_FORMAT(archived_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS archived_at
         FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(branch_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work branch", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let string = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))
    };
    let optional = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))
    };
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))
    };
    WorkBranchRecord::from_parts(WorkBranchRecordParts {
        work_id: super::WorkId::parse(string("work_id")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        branch_id: super::WorkBranchId::parse(string("branch_id")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        branch_revision: WorkBranchRevision::new(integer("branch_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        session_id: super::InternalSessionId::parse(string("session_id")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        origin_branch_id: optional("origin_branch_id")?
            .map(super::WorkBranchId::parse)
            .transpose()
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        fork_cursor: optional("fork_cursor")?
            .map(ForkCursorRef::parse)
            .transpose()
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        goal_revision_ref: super::GoalRevision::new(integer("goal_revision_ref")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        criteria_set_revision_ref: super::CriterionSetRevision::new(integer(
            "criteria_set_revision_ref",
        )?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        basis_graph_revision: GraphRevision::new(integer("basis_graph_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        current_graph_revision: GraphRevision::new(integer("current_graph_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?,
        created_at: decode_timestamp(string("created_at")?)?,
        archived_at: optional("archived_at")?.map(decode_timestamp).transpose()?,
    })
    .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))
}

async fn load_branch(
    transaction: &mut Transaction<'_, MySql>,
    change: &WorkGraphChange,
) -> Result<WorkBranchRecord, WorkRepositoryError> {
    load_branch_by_identity(
        transaction,
        &change.owner_id,
        &change.work_id,
        &change.branch_id,
    )
    .await
}

pub(super) struct PreparedGraphChange {
    change: WorkGraphChange,
    graph: CanonicalGraph,
    encoded: EncodedGraph,
    next_branch_revision: WorkBranchRevision,
}

/// Retire execution carriers bound to revisions replaced by this graph
/// change. Graph admission and attempt invalidation are one transaction: a
/// caller must never observe a newly cancelled/superseded item while an old
/// revision remains the branch's runnable foreground authority.
pub(super) async fn retire_revised_item_attempts(
    transaction: &mut Transaction<'_, MySql>,
    prepared: &PreparedGraphChange,
) -> Result<u64, WorkRepositoryError> {
    let revised = prepared
        .change
        .items
        .iter()
        .filter_map(|item| match item {
            WorkGraphItemChange::Revised(item) => Some(item.expected_ref()),
            WorkGraphItemChange::Existing(_) | WorkGraphItemChange::New(_) => None,
        })
        .collect::<Vec<_>>();
    if revised.is_empty() {
        return Ok(0);
    }

    let mut update = QueryBuilder::<MySql>::new(
        "UPDATE work_item_attempts SET status = 'cancelled', updated_at = CURRENT_TIMESTAMP(6) \
         WHERE owner_id = ",
    );
    update
        .push_bind(prepared.change.owner_id.as_str())
        .push(" AND work_id = ")
        .push_bind(prepared.change.work_id.as_str())
        .push(" AND branch_id = ")
        .push_bind(prepared.change.branch_id.as_str())
        .push(" AND outcome IS NULL AND status IN ('running', 'waiting', 'paused') AND (");
    append_revised_attempt_predicates(&mut update, &revised);
    update.push(")");
    update
        .build()
        .execute(&mut **transaction)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| {
            WorkRepositoryError::persistence("retire revised WorkItem attempts", source)
        })
}

fn append_revised_attempt_predicates(
    update: &mut QueryBuilder<'_, MySql>,
    revised: &[super::WorkItemRevisionRef],
) {
    for (index, item) in revised.iter().enumerate() {
        if index > 0 {
            update.push(" OR ");
        }
        update
            .push("(work_item_id = ")
            .push_bind(item.item_id.as_str().to_string())
            .push(" AND work_item_revision = ")
            .push_bind(item.revision.get())
            .push(")");
    }
}

pub(super) async fn prepare_graph_change(
    transaction: &mut Transaction<'_, MySql>,
    mut change: WorkGraphChange,
) -> Result<PreparedGraphChange, WorkRepositoryError> {
    verify_revised_item_bases(transaction, &change).await?;
    allocate_revised_item_revisions(transaction, &mut change).await?;
    let graph = validate_and_canonicalize_graph(&change.items, &change.edges)
        .map_err(super::repository::invalid_mutation)?;
    let encoded = encode_graph(&graph, &change)?;
    let next_branch_revision = change
        .expected_branch_revision
        .checked_next()
        .map_err(super::repository::invalid_mutation)?;
    Ok(PreparedGraphChange {
        change,
        graph,
        encoded,
        next_branch_revision,
    })
}

pub(super) async fn apply_prepared_graph_change(
    transaction: &mut Transaction<'_, MySql>,
    prepared: &PreparedGraphChange,
    actor_kind: &str,
    actor_id: &str,
    patch_hash: Option<&str>,
) -> Result<WorkBranchRecord, WorkRepositoryError> {
    let change = &prepared.change;
    let graph = &prepared.graph;
    let encoded = &prepared.encoded;
    let next_branch_revision = prepared.next_branch_revision;
    let item_count = i64::try_from(graph.item_refs.len()).expect("bounded Work graph items");
    let edge_count = i64::try_from(graph.edges.len()).expect("bounded Work graph edges");
    let work = query(
        "SELECT CASE WHEN archived_at IS NULL THEN 0 ELSE 1 END AS is_archived
         FROM works WHERE owner_id = ? AND work_id = ? LIMIT 1",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load graph Work", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    if work
        .try_get::<i64, _>("is_archived")
        .map_err(|source| WorkRepositoryError::corrupt("Work", source))?
        != 0
    {
        return Err(WorkRepositoryError::Archived);
    }

    let allocated = query(
        "UPDATE work_graph_sequences SET last_revision = last_revision + 1
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .execute(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("allocate graph revision", source))?;
    if allocated.rows_affected() != 1 {
        return Err(WorkRepositoryError::corrupt(
            "Work graph sequence",
            std::io::Error::other("missing or duplicate graph sequence row"),
        ));
    }
    let allocated_revision: i64 = query(
        "SELECT last_revision FROM work_graph_sequences
         WHERE owner_id = ? AND work_id = ? LIMIT 1",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("read graph revision", source))?
    .try_get("last_revision")
    .map_err(|source| WorkRepositoryError::corrupt("Work graph sequence", source))?;
    let graph_revision = GraphRevision::new(allocated_revision)
        .map_err(|source| WorkRepositoryError::corrupt("Work graph sequence", source))?;

    let branch_update = query(
        "UPDATE work_branches
         SET branch_revision = ?, current_graph_revision = ?, updated_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND branch_revision = ? AND current_graph_revision = ?
           AND archived_at IS NULL",
    )
    .bind(next_branch_revision.get())
    .bind(graph_revision.get())
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(change.branch_id.as_str())
    .bind(change.expected_branch_revision.get())
    .bind(change.expected_graph_revision.get())
    .execute(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("advance branch graph CAS", source))?;
    if branch_update.rows_affected() == 0 {
        let current = load_branch(transaction, change).await?;
        if current.parts().archived_at.is_some() {
            return Err(WorkRepositoryError::Archived);
        }
        return Err(WorkRepositoryError::StaleGraphRevision {
            expected_branch_revision: change.expected_branch_revision,
            actual_branch_revision: current.parts().branch_revision,
            expected_graph_revision: change.expected_graph_revision,
            actual_graph_revision: current.parts().current_graph_revision,
        });
    }
    if branch_update.rows_affected() != 1 {
        return Err(WorkRepositoryError::corrupt(
            "Work branch graph CAS",
            std::io::Error::other("owner-scoped graph CAS updated multiple branches"),
        ));
    }

    verify_existing_item_revisions(transaction, change).await?;
    insert_new_items(transaction, change).await?;
    insert_revised_items(transaction, change).await?;
    insert_edges(transaction, change, graph, graph_revision).await?;
    query(
        "INSERT INTO work_graph_revisions
         (owner_id, work_id, revision, parent_revision, item_revision_manifest_json,
          edge_manifest_json, manifest_hash, item_count, edge_count, patch_ref, patch_hash,
          actor_kind, actor_id, reason)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(graph_revision.get())
    .bind(change.expected_graph_revision.get())
    .bind(&encoded.items_json)
    .bind(&encoded.edges_json)
    .bind(&encoded.manifest_hash)
    .bind(item_count)
    .bind(edge_count)
    .bind(change.source_ref.as_str())
    .bind(patch_hash.unwrap_or(&encoded.replacement_hash))
    .bind(actor_kind)
    .bind(actor_id)
    .bind(change.reason.as_ref().map(super::WorkChangeReason::as_str))
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert graph revision",
            WorkConflictResource::GraphRevision,
            source,
        )
    })?;

    let event_result = super::events_repository::append_event(
        transaction,
        &super::events::NewWorkEvent {
            owner_id: change.owner_id.clone(),
            work_id: change.work_id.clone(),
            branch_id: Some(change.branch_id.clone()),
            kind: super::WorkEventKind::GraphReplaced,
            work_revision: None,
            goal_revision: None,
            criterion_set_revision: None,
            branch_revision: Some(next_branch_revision),
            graph_revision: Some(graph_revision),
            source_ref: change.source_ref.clone(),
        },
    )
    .await;
    event_result?;
    load_branch(transaction, change).await
}

pub(super) async fn replace_graph(
    repository: &DatabaseWorkRepository,
    change: WorkGraphChange,
) -> Result<WorkBranchRecord, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin graph revision transaction", source)
    })?;
    let prepared = match prepare_graph_change(&mut transaction, change).await {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(super::repository::rollback_transaction(
                transaction,
                "rollback graph preparation transaction",
                error,
            )
            .await);
        }
    };
    let actor_id = prepared.change.owner_id.as_str();
    let updated = match apply_prepared_graph_change(
        &mut transaction,
        &prepared,
        "user",
        actor_id,
        None,
    )
    .await
    {
        Ok(updated) => updated,
        Err(error) => {
            return Err(super::repository::rollback_transaction(
                transaction,
                "rollback graph revision transaction",
                error,
            )
            .await);
        }
    };
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit graph revision transaction", source)
    })?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{
        NewWorkItem, WorkBranchId, WorkChangeRef, WorkGraphItemChange, WorkId,
        WorkItemDeclarationState, WorkItemEdge, WorkItemEdgeKind, WorkItemRevisionChange,
        WorkOwnerId,
    };

    fn new_item(item_id: &str, objective: &str) -> WorkGraphItemChange {
        WorkGraphItemChange::New(NewWorkItem {
            item_id: WorkItemId::parse(item_id).expect("item id"),
            kind: WorkItemKind::Task,
            objective: WorkItemText::parse(objective).expect("objective"),
            expected_result: WorkItemText::parse(format!("{item_id} is proven"))
                .expect("expected result"),
        })
    }

    fn change(items: Vec<WorkGraphItemChange>, edges: Vec<WorkItemEdge>) -> WorkGraphChange {
        WorkGraphChange {
            owner_id: WorkOwnerId::parse("owner-1").expect("owner"),
            work_id: WorkId::parse("work-1").expect("work"),
            branch_id: WorkBranchId::parse("branch-1").expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items,
            edges,
            source_ref: WorkChangeRef::parse("event-1").expect("source"),
            reason: None,
        }
    }

    fn dependency(predecessor: &str, successor: &str) -> WorkItemEdge {
        WorkItemEdge {
            predecessor_item_id: WorkItemId::parse(predecessor).expect("predecessor"),
            successor_item_id: WorkItemId::parse(successor).expect("successor"),
            kind: WorkItemEdgeKind::Dependency,
        }
    }

    fn encode(change: &WorkGraphChange) -> EncodedGraph {
        let graph = validate_and_canonicalize_graph(&change.items, &change.edges).expect("graph");
        encode_graph(&graph, change).expect("encoding")
    }

    #[test]
    fn retired_attempt_predicates_keep_each_revision_pair_atomic() {
        let revised = [
            super::super::WorkItemRevisionRef {
                item_id: WorkItemId::parse("task-1").expect("item"),
                revision: WorkItemRevision::new(1).expect("revision"),
            },
            super::super::WorkItemRevisionRef {
                item_id: WorkItemId::parse("task-2").expect("item"),
                revision: WorkItemRevision::new(2).expect("revision"),
            },
        ];
        let mut query = QueryBuilder::<MySql>::new("WHERE (");
        append_revised_attempt_predicates(&mut query, &revised);
        query.push(")");
        assert_eq!(
            query.sql(),
            "WHERE ((work_item_id = ? AND work_item_revision = ?) OR (work_item_id = ? AND work_item_revision = ?))"
        );
    }

    #[test]
    fn replacement_hash_is_canonical_and_binds_new_item_definitions() {
        let first = change(
            vec![new_item("b", "Build B"), new_item("a", "Build A")],
            vec![dependency("a", "b")],
        );
        let reordered = change(
            vec![new_item("a", "Build A"), new_item("b", "Build B")],
            vec![dependency("a", "b")],
        );
        let changed_definition = change(
            vec![
                new_item("a", "Build A differently"),
                new_item("b", "Build B"),
            ],
            vec![dependency("a", "b")],
        );

        let first = encode(&first);
        let reordered = encode(&reordered);
        let changed_definition = encode(&changed_definition);
        assert_eq!(first.manifest_hash, reordered.manifest_hash);
        assert_eq!(first.replacement_hash, reordered.replacement_hash);
        assert_eq!(first.manifest_hash, changed_definition.manifest_hash);
        assert_ne!(first.replacement_hash, changed_definition.replacement_hash);
    }

    #[test]
    fn replacement_hash_binds_revised_item_content_and_declaration_state() {
        let revised = |state| {
            let mut revision = WorkItemRevisionChange::new(
                WorkItemId::parse("task-a").expect("item"),
                WorkItemRevision::INITIAL,
                WorkItemKind::Task,
                WorkItemText::parse("Revised objective").expect("objective"),
                WorkItemText::parse("Revised result").expect("result"),
                state,
            );
            revision.assign_result_revision(WorkItemRevision::new(2).expect("revision"));
            change(vec![WorkGraphItemChange::Revised(revision)], Vec::new())
        };
        let active = encode(&revised(WorkItemDeclarationState::Active));
        let cancelled = encode(&revised(WorkItemDeclarationState::Cancelled));
        assert_eq!(
            active.manifest_hash, cancelled.manifest_hash,
            "the graph manifest intentionally addresses only the resulting item revision"
        );
        assert_ne!(
            active.replacement_hash, cancelled.replacement_hash,
            "the accepted patch hash must bind the declaration transition"
        );
    }
}
