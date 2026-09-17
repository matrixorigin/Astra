//! Compile immutable semantic admission into Work graph operations. Identity,
//! explicit precedence and graph-application timing are independent facts.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    GraphRevision, WorkItemAttemptId, WorkItemId, WorkItemRevision, WorkItemRevisionRef,
    WorkProposalId, WorkProposalInvocationIdentity, WorkProposalKind,
};
use crate::{WorkAdmissionDecision, WorkAdmissionGraphMutation, WorkAdmissionTask};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkEstablishmentItem {
    pub item_id: String,
    pub kind: &'static str,
    pub objective: String,
    pub expected_result: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct WorkEstablishmentDependency {
    pub predecessor_item_id: String,
    pub successor_item_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkEstablishmentItemRevision {
    pub item_id: String,
    pub expected_revision: i64,
    pub kind: &'static str,
    pub objective: String,
    pub expected_result: String,
    pub declaration_state: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkEstablishmentMutationGroup {
    pub operation_id: String,
    pub tool_call_id: String,
    pub after_initial_tasks: Vec<usize>,
    pub trigger_items: Vec<WorkItemRevisionRef>,
    pub additions: Vec<WorkEstablishmentItem>,
    pub revisions: Vec<WorkEstablishmentItemRevision>,
    pub dependencies: Vec<WorkEstablishmentDependency>,
    pub dependency_removals: Vec<WorkEstablishmentDependency>,
}

/// A deferred admission mutation that has already been accepted into the
/// canonical graph.  The group is immutable admission data; the graph
/// revision is the durable result recorded by the accepted plan proposal.
/// Keeping both in the execution snapshot lets recovery publish the same
/// mutation fact after a response was lost, without scanning proposal history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkAppliedGraphMutation {
    pub group: WorkEstablishmentMutationGroup,
    pub result_graph_revision: GraphRevision,
    /// The exact primary attempt whose settlement made this mutation eligible.
    /// Immediate admission mutations have no trigger attempt.
    pub trigger_attempt_id: Option<WorkItemAttemptId>,
    /// The initial item reference recorded beside the trigger attempt.  This
    /// is retained as an integrity fact even though runtime receipts usually
    /// match the stronger attempt identity.
    pub trigger_item: Option<WorkItemRevisionRef>,
    /// `false` means the accepted graph revision predates the trigger
    /// association (or the association was otherwise unavailable). Runtime
    /// receipts must then publish the cumulative accepted set instead of
    /// pretending that the current settlement caused it.
    pub trigger_association_known: bool,
}

impl WorkEstablishmentMutationGroup {
    pub fn proposal_id(
        &self,
        owner_id: &str,
        session_id: &str,
        work_id: &str,
        branch_id: &str,
    ) -> Result<WorkProposalId, String> {
        WorkProposalInvocationIdentity {
            owner_id,
            session_id,
            work_id,
            branch_id,
            run_id: &self.operation_id,
            turn_chain_id: &self.operation_id,
            tool_call_id: &self.tool_call_id,
        }
        .proposal_id(WorkProposalKind::PlanPatch)
    }

    pub fn proposal_arguments(&self, context_id: &str) -> Value {
        serde_json::json!({
            "context_id": context_id,
            "reason": "Apply the persisted Work admission graph decision",
            "additions": self.additions,
            "revisions": self.revisions,
            "dependencies": self.dependencies,
            "dependency_removals": self.dependency_removals,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkEstablishmentPlan {
    pub initial_items: Vec<WorkEstablishmentItem>,
    pub initial_dependencies: Vec<WorkEstablishmentDependency>,
    pub mutation_groups: Vec<WorkEstablishmentMutationGroup>,
}

/// One shared decoder for runtime recovery and the service scheduler barrier.
pub fn decode_work_establishment_payload(
    payload_json: &str,
) -> Result<(Value, Option<WorkAdmissionDecision>), String> {
    let payload: Value = serde_json::from_str(payload_json)
        .map_err(|error| format!("invalid canonical Work payload: {error}"))?;
    if payload.get("schema_version").and_then(Value::as_u64) != Some(2) {
        return Err("unsupported canonical Work payload schema".into());
    }
    let arguments = payload
        .get("arguments")
        .cloned()
        .ok_or("canonical Work payload has no start_work arguments")?;
    let decision: Option<WorkAdmissionDecision> = payload
        .get("admission_decision")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| format!("invalid persisted Work admission decision: {error}"))?;
    if let Some((_, tasks)) = decision
        .as_ref()
        .and_then(WorkAdmissionDecision::initial_work_plan)
    {
        let argument_tasks: Vec<WorkAdmissionTask> = serde_json::from_value(
            arguments
                .get("tasks")
                .cloned()
                .ok_or("canonical Work arguments have no tasks")?,
        )
        .map_err(|error| format!("invalid canonical Work tasks: {error}"))?;
        if argument_tasks != tasks {
            return Err("canonical Work arguments disagree with admission tasks".into());
        }
    }
    Ok((arguments, decision))
}

fn validate_refs(refs: &[usize], count: usize) -> Result<(), String> {
    if refs.len() > count
        || refs.iter().any(|n| *n == 0 || *n > count)
        || refs.iter().collect::<BTreeSet<_>>().len() != refs.len()
    {
        return Err("Work precedence references must be unique initial task indices".into());
    }
    Ok(())
}

fn task_item(item_id: String, task: &WorkAdmissionTask) -> WorkEstablishmentItem {
    WorkEstablishmentItem {
        item_id,
        kind: "task",
        objective: task.objective.clone(),
        expected_result: task.expected_result.clone(),
    }
}

pub fn compile_initial_work_establishment_graph(
    tasks: &[WorkAdmissionTask],
) -> Result<(Vec<WorkEstablishmentItem>, Vec<WorkEstablishmentDependency>), String> {
    let plan = compile_work_establishment_plan("initial-graph", tasks, &[])?;
    Ok((plan.initial_items, plan.initial_dependencies))
}

/// IDs remain byte-for-byte compatible with existing immediate mutations.
/// Trigger groups never renumber additions or infer an execution chain.
pub fn compile_work_establishment_plan(
    operation_id: &str,
    tasks: &[WorkAdmissionTask],
    mutations: &[WorkAdmissionGraphMutation],
) -> Result<WorkEstablishmentPlan, String> {
    if operation_id.trim().is_empty() || tasks.is_empty() || tasks.len() + mutations.len() > 8 {
        return Err("Work establishment exceeds its bounded admission contract".into());
    }
    let count = tasks.len();
    let mut ancestors = vec![BTreeSet::new(); count];
    for (index, task) in tasks.iter().enumerate() {
        validate_refs(&task.after_initial_tasks, count)?;
        if task.after_initial_tasks.contains(&(index + 1)) {
            return Err("initial Work task cannot depend on itself".into());
        }
        ancestors[index].extend(task.after_initial_tasks.iter().copied());
    }
    for _ in 0..count {
        let before = ancestors.clone();
        for refs in &mut ancestors {
            for reference in refs.clone() {
                refs.extend(before[reference - 1].iter().copied());
            }
        }
    }
    if ancestors
        .iter()
        .enumerate()
        .any(|(index, refs)| refs.contains(&(index + 1)))
    {
        return Err("initial Work task dependencies contain a cycle".into());
    }
    let mut retirements = BTreeMap::new();
    for mutation in mutations {
        validate_refs(mutation.after_initial_tasks(), count)?;
        if let Some(addition) = mutation.addition() {
            validate_refs(&addition.after_initial_tasks, count)?;
        }
        if let Some(target) = mutation.target_initial_candidate() {
            if mutation.after_initial_tasks().contains(&target) {
                return Err(
                    "a mutation cannot retire the candidate whose delivery triggers it".into(),
                );
            }
            if target == 0 || target > count || mutation.retirement() != Some(&tasks[target - 1]) {
                return Err(
                    "persisted Work retirement conflicts with its initial candidate".into(),
                );
            }
            if retirements.insert(target, mutation).is_some() {
                return Err("one initial Work candidate cannot be retired twice".into());
            }
        }
    }
    // A referenced delivery must be guaranteed before candidate retirement.
    // Waiting for a descendant proves that fact; incidental execution order
    // among independent tasks does not.
    for mutation in mutations {
        for target in mutation.after_initial_tasks() {
            if let Some(retirement) = retirements.get(target)
                && !retirement
                    .after_initial_tasks()
                    .iter()
                    .any(|n| ancestors[n - 1].contains(target))
            {
                return Err(
                    "Work mutation trigger delivery is not guaranteed before candidate retirement"
                        .into(),
                );
            }
        }
    }

    let same_trigger = |left: &[usize], right: &[usize]| {
        left.iter().copied().collect::<BTreeSet<_>>()
            == right.iter().copied().collect::<BTreeSet<_>>()
    };
    // Initial candidate references are not mutable "latest replacement"
    // aliases. Do not fabricate ordering across independent trigger groups.
    for mutation in mutations {
        let mut dependencies = mutation
            .addition()
            .map(|task| task.after_initial_tasks.clone())
            .unwrap_or_default();
        if let WorkAdmissionGraphMutation::Replace { target, .. } = mutation {
            dependencies.extend(&target.after_initial_tasks);
        }
        for predecessor in dependencies {
            if let Some(retirement) = retirements.get(&predecessor)
                && (!matches!(retirement, WorkAdmissionGraphMutation::Replace { .. })
                    || !same_trigger(
                        retirement.after_initial_tasks(),
                        mutation.after_initial_tasks(),
                    ))
            {
                return Err(
                    "task prerequisite crosses an incompatible candidate retirement".into(),
                );
            }
        }
    }
    for (index, task) in tasks.iter().enumerate() {
        for predecessor in &task.after_initial_tasks {
            if let (Some(before), Some(after)) =
                (retirements.get(predecessor), retirements.get(&(index + 1)))
                && !same_trigger(before.after_initial_tasks(), after.after_initial_tasks())
            {
                return Err(
                    "dependent candidate retirements require one atomic trigger group".into(),
                );
            }
        }
    }

    let initial_items = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| task_item(format!("task-{}", index + 1), task))
        .collect::<Vec<_>>();
    let initial_dependencies = tasks
        .iter()
        .enumerate()
        .flat_map(|(index, task)| {
            task.after_initial_tasks
                .iter()
                .map(move |predecessor| WorkEstablishmentDependency {
                    predecessor_item_id: format!("task-{predecessor}"),
                    successor_item_id: format!("task-{}", index + 1),
                })
        })
        .collect::<Vec<_>>();
    let mutation_operation_id = format!("{operation_id}-mutations");
    let mut hasher = Sha256::new();
    hasher.update(b"continuation-work-items-v1\0");
    hasher.update(mutation_operation_id.as_bytes());
    let namespace = format!("{:x}", hasher.finalize());
    let mut addition_index = 0;
    let mut groups = BTreeMap::<Vec<usize>, WorkEstablishmentMutationGroup>::new();
    let mut replacements = BTreeMap::<Vec<usize>, BTreeMap<String, String>>::new();
    for mutation in mutations {
        let mut triggers = mutation.after_initial_tasks().to_vec();
        triggers.sort_unstable();
        let group = groups.entry(triggers.clone()).or_insert_with(|| {
            let tool_call_id = if triggers.is_empty() {
                mutation_operation_id.clone()
            } else {
                let mut hasher = Sha256::new();
                hasher.update(b"work-admission-trigger-v1\0");
                for trigger in &triggers {
                    hasher.update((*trigger as u64).to_be_bytes());
                }
                format!(
                    "{mutation_operation_id}-after-{}",
                    &format!("{:x}", hasher.finalize())[..24]
                )
            };
            WorkEstablishmentMutationGroup {
                operation_id: operation_id.to_string(),
                tool_call_id,
                trigger_items: triggers
                    .iter()
                    .map(|index| WorkItemRevisionRef {
                        item_id: WorkItemId::parse(format!("task-{index}"))
                            .expect("allocated task identity"),
                        revision: WorkItemRevision::INITIAL,
                    })
                    .collect(),
                after_initial_tasks: triggers,
                additions: Vec::new(),
                revisions: Vec::new(),
                dependencies: Vec::new(),
                dependency_removals: Vec::new(),
            }
        });
        let mut replacement_id = None;
        if let Some(addition) = mutation.addition() {
            addition_index += 1;
            let item_id = format!("task-{}-{addition_index}", &namespace[..48]);
            group.additions.push(task_item(item_id.clone(), addition));
            let mut dependencies = addition
                .after_initial_tasks
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            if let WorkAdmissionGraphMutation::Replace { target, .. } = mutation {
                dependencies.extend(target.after_initial_tasks.iter().copied());
                replacement_id = Some(item_id.clone());
                replacements
                    .entry(group.after_initial_tasks.clone())
                    .or_default()
                    .insert(
                        format!(
                            "task-{}",
                            mutation
                                .target_initial_candidate()
                                .expect("replacement target")
                        ),
                        item_id.clone(),
                    );
            }
            for predecessor in dependencies {
                if mutation.target_initial_candidate() == Some(predecessor) {
                    return Err("replacement cannot depend on the candidate it supersedes".into());
                }
                group.dependencies.push(WorkEstablishmentDependency {
                    predecessor_item_id: format!("task-{predecessor}"),
                    successor_item_id: item_id.clone(),
                });
            }
        }
        if let Some(target) = mutation.target_initial_candidate() {
            let candidate = &initial_items[target - 1];
            group.revisions.push(WorkEstablishmentItemRevision {
                item_id: candidate.item_id.clone(),
                expected_revision: WorkItemRevision::INITIAL.get(),
                kind: candidate.kind,
                objective: candidate.objective.clone(),
                expected_result: candidate.expected_result.clone(),
                declaration_state: mutation
                    .required_declaration_state()
                    .expect("retirement state"),
            });
            for edge in initial_dependencies
                .iter()
                .filter(|edge| edge.predecessor_item_id == candidate.item_id)
            {
                if let Some(replacement_id) = &replacement_id {
                    group.dependency_removals.push(edge.clone());
                    group.dependencies.push(WorkEstablishmentDependency {
                        predecessor_item_id: replacement_id.clone(),
                        successor_item_id: edge.successor_item_id.clone(),
                    });
                } else if !retirements
                    .keys()
                    .any(|retired| edge.successor_item_id == format!("task-{retired}"))
                {
                    return Err(
                        "cancelled initial prerequisite would strand an active successor".into(),
                    );
                }
            }
        }
    }
    let mutation_groups = groups
        .into_values()
        .map(|mut group| {
            if let Some(substitutions) = replacements.get(&group.after_initial_tasks) {
                for edge in &mut group.dependencies {
                    if let Some(replacement) = substitutions.get(&edge.predecessor_item_id) {
                        edge.predecessor_item_id = replacement.clone();
                    }
                    if let Some(replacement) = substitutions.get(&edge.successor_item_id) {
                        edge.successor_item_id = replacement.clone();
                    }
                }
            }
            group.additions.sort_by(|a, b| a.item_id.cmp(&b.item_id));
            group.revisions.sort_by(|a, b| a.item_id.cmp(&b.item_id));
            group.dependencies.sort();
            group.dependencies.dedup();
            group.dependency_removals.sort();
            group.dependency_removals.dedup();
            let retired = group
                .revisions
                .iter()
                .map(|revision| revision.item_id.as_str())
                .collect::<BTreeSet<_>>();
            let mut graph = BTreeMap::<&str, BTreeSet<&str>>::new();
            for edge in initial_dependencies
                .iter()
                .filter(|edge| !group.dependency_removals.contains(edge))
                .chain(&group.dependencies)
            {
                if !retired.contains(edge.predecessor_item_id.as_str())
                    && !retired.contains(edge.successor_item_id.as_str())
                {
                    graph
                        .entry(&edge.successor_item_id)
                        .or_default()
                        .insert(&edge.predecessor_item_id);
                }
            }
            for _ in 0..(initial_items.len() + group.additions.len()) {
                let before = graph.clone();
                for predecessors in graph.values_mut() {
                    for predecessor in predecessors.clone() {
                        if let Some(ancestors) = before.get(predecessor) {
                            predecessors.extend(ancestors);
                        }
                    }
                }
            }
            if graph
                .iter()
                .any(|(item, predecessors)| predecessors.contains(item))
            {
                return Err("Work mutation creates cyclic task prerequisites".to_string());
            }
            Ok(group)
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(WorkEstablishmentPlan {
        initial_items,
        initial_dependencies,
        mutation_groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(label: &str, after: &[usize]) -> WorkAdmissionTask {
        WorkAdmissionTask {
            objective: label.into(),
            expected_result: format!("Evidence for {label}"),
            after_initial_tasks: after.to_vec(),
        }
    }

    #[test]
    fn establishment_compiler_keeps_ids_precedence_and_mutation_timing_independent() {
        let tasks = vec![task("First", &[]), task("Second", &[1])];
        let mutations = vec![
            WorkAdmissionGraphMutation::Cancel {
                target_initial_candidate: 2,
                target: tasks[1].clone(),
                after_initial_tasks: vec![1],
            },
            WorkAdmissionGraphMutation::Add {
                task: task("Later", &[]),
                after_initial_tasks: vec![1],
            },
        ];
        let plan = (0..256)
            .find_map(|index| {
                let plan =
                    compile_work_establishment_plan(&format!("op-{index}"), &tasks, &mutations)
                        .unwrap();
                (plan.mutation_groups[0].additions[0].item_id < plan.initial_items[0].item_id)
                    .then_some(plan)
            })
            .expect("fixture addition sorts before task-1");
        assert_eq!(plan.initial_items.len(), 2);
        assert_eq!(
            plan.initial_dependencies,
            vec![WorkEstablishmentDependency {
                predecessor_item_id: "task-1".into(),
                successor_item_id: "task-2".into(),
            }]
        );
        let group = &plan.mutation_groups[0];
        assert_eq!(group.after_initial_tasks, vec![1]);
        assert_eq!(group.trigger_items[0].item_id.as_str(), "task-1");
        assert_eq!(group.revisions[0].item_id, "task-2");
        assert!(
            group.dependencies.is_empty(),
            "mutation timing does not fabricate task edges"
        );
        assert_eq!(
            compile_work_establishment_plan(&group.operation_id, &tasks, &mutations).unwrap(),
            plan
        );
    }

    #[test]
    fn establishment_compiler_replacement_inherits_precedence_without_serializing_peers() {
        let tasks = vec![
            task("First", &[]),
            task("Second", &[1]),
            task("Independent", &[]),
        ];
        let mutations = vec![WorkAdmissionGraphMutation::Replace {
            target_initial_candidate: 2,
            target: tasks[1].clone(),
            replacement: task("Replacement", &[]),
            after_initial_tasks: vec![],
        }];
        let plan = compile_work_establishment_plan("op", &tasks, &mutations).unwrap();
        assert_eq!(plan.initial_dependencies.len(), 1);
        let group = &plan.mutation_groups[0];
        assert_eq!(group.tool_call_id, "op-mutations");
        assert_eq!(
            group.dependencies,
            vec![WorkEstablishmentDependency {
                predecessor_item_id: "task-1".into(),
                successor_item_id: group.additions[0].item_id.clone(),
            }]
        );
        assert!(
            !group
                .dependencies
                .iter()
                .any(|edge| edge.successor_item_id == "task-3")
        );
    }

    #[test]
    fn establishment_compiler_substitutes_both_endpoints_of_same_group_replacements() {
        let tasks = vec![task("First", &[]), task("Second", &[1])];
        let mutations = vec![
            WorkAdmissionGraphMutation::Replace {
                target_initial_candidate: 1,
                target: tasks[0].clone(),
                replacement: task("New first", &[]),
                after_initial_tasks: vec![],
            },
            WorkAdmissionGraphMutation::Replace {
                target_initial_candidate: 2,
                target: tasks[1].clone(),
                replacement: task("New second", &[]),
                after_initial_tasks: vec![],
            },
            WorkAdmissionGraphMutation::Add {
                task: task("Added", &[1]),
                after_initial_tasks: vec![],
            },
        ];
        let plan = compile_work_establishment_plan("op", &tasks, &mutations).unwrap();
        let group = &plan.mutation_groups[0];
        let id = |label| {
            group
                .additions
                .iter()
                .find(|item| item.objective == label)
                .unwrap()
                .item_id
                .clone()
        };
        assert_eq!(
            group.dependencies,
            vec![
                WorkEstablishmentDependency {
                    predecessor_item_id: id("New first"),
                    successor_item_id: id("New second")
                },
                WorkEstablishmentDependency {
                    predecessor_item_id: id("New first"),
                    successor_item_id: id("Added")
                },
            ]
        );
    }

    #[test]
    fn establishment_compiler_rejects_cross_trigger_retired_prerequisites() {
        let tasks = vec![task("First", &[]), task("Second", &[])];
        let mutations = vec![
            WorkAdmissionGraphMutation::Replace {
                target_initial_candidate: 1,
                target: tasks[0].clone(),
                replacement: task("New first", &[]),
                after_initial_tasks: vec![],
            },
            WorkAdmissionGraphMutation::Add {
                task: task("Added", &[1]),
                after_initial_tasks: vec![2],
            },
        ];
        assert!(compile_work_establishment_plan("op", &tasks, &mutations).is_err());
    }

    #[test]
    fn establishment_compiler_keeps_global_addition_ids_across_trigger_groups() {
        let tasks = vec![task("First", &[]), task("Second", &[])];
        let mutations = vec![
            WorkAdmissionGraphMutation::Add {
                task: task("A", &[]),
                after_initial_tasks: vec![2, 1],
            },
            WorkAdmissionGraphMutation::Add {
                task: task("B", &[]),
                after_initial_tasks: vec![],
            },
        ];
        let plan = compile_work_establishment_plan("op", &tasks, &mutations).unwrap();
        let immediate = &plan.mutation_groups[0];
        let deferred = &plan.mutation_groups[1];
        assert!(immediate.additions[0].item_id.ends_with("-2"));
        assert!(deferred.additions[0].item_id.ends_with("-1"));
        assert_eq!(deferred.after_initial_tasks, vec![1, 2]);
        let mut reordered = mutations.clone();
        if let WorkAdmissionGraphMutation::Add {
            after_initial_tasks,
            ..
        } = &mut reordered[0]
        {
            after_initial_tasks.reverse();
        }
        assert_eq!(
            compile_work_establishment_plan("op", &tasks, &reordered).unwrap(),
            plan
        );
    }

    #[test]
    fn establishment_compiler_rejects_impossible_triggers_and_cyclic_initial_tasks() {
        let tasks = vec![task("First", &[]), task("Second", &[1])];
        let mutations = vec![
            WorkAdmissionGraphMutation::Cancel {
                target_initial_candidate: 2,
                target: tasks[1].clone(),
                after_initial_tasks: vec![1],
            },
            WorkAdmissionGraphMutation::Add {
                task: task("Later", &[]),
                after_initial_tasks: vec![2],
            },
        ];
        assert!(compile_work_establishment_plan("op", &tasks, &mutations).is_err());
        assert!(
            compile_initial_work_establishment_graph(&[task("A", &[2]), task("B", &[1])]).is_err()
        );
        assert!(compile_initial_work_establishment_graph(&[task("A", &[0])]).is_err());
        assert!(compile_initial_work_establishment_graph(&[task("A", &[1])]).is_err());
    }
}
