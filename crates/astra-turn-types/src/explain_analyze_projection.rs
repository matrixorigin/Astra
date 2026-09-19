//! Incremental, transport-independent projection of Explain Analyze facts.
//!
//! This reducer owns only the facts carried by `ExplainAnalyzeEventV1`. It does
//! not reconstruct execution from traces and deliberately keeps provider usage
//! separate from request/context estimates.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{
    EXPLAIN_ANALYZE_EVENT_TYPE, ExplainAnalyzeContextMetricsV1, ExplainAnalyzeEventV1,
    ExplainAnalyzeNodeKindV1, ExplainAnalyzeOutcomeV1, ExplainAnalyzeTokenUsageV1,
    ExplainAnalyzeTransitionV1,
};

/// A projected node in stable first-seen order. The index of a node in
/// [`ExplainAnalyzeGraphV1::nodes`] remains stable as more facts are applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainAnalyzeProjectedNodeV1 {
    pub node_id: String,
    pub run_id: String,
    pub turn_id: String,
    pub clock_domain_id: String,
    pub kind: ExplainAnalyzeNodeKindV1,
    pub label: String,
    pub parent_node_id: Option<String>,
    /// Resolved index of `parent_node_id`; a missing parent is kept as a root
    /// until its fact arrives.
    pub parent_index: Option<usize>,
    pub dependency_node_ids: Vec<String>,
    /// Resolved dependency indices, aligned with `dependency_node_ids`.
    pub dependency_indices: Vec<Option<usize>>,
    pub round_index: Option<u32>,
    pub attempt_index: Option<u32>,
    pub start_elapsed_ms: u64,
    pub end_elapsed_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub outcome: Option<ExplainAnalyzeOutcomeV1>,
    pub usage: Option<ExplainAnalyzeTokenUsageV1>,
    pub context: Option<ExplainAnalyzeContextMetricsV1>,
    pub auxiliary_usage: Option<Box<crate::ExplainAnalyzeAuxiliaryUsageV1>>,
    pub coverage_gaps: Vec<crate::ExplainAnalyzeCoverageGapV1>,
    pub start_observed: bool,
    pub terminal_observed: bool,
    pub conflicted: bool,
}

/// Whether all known facts are internally consistent. `Unknown` also covers a
/// graph whose cycle check has not yet been finalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainAnalyzeGraphIntegrityV1 {
    Consistent,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeProjectionDiagnosticCodeV1 {
    ConflictingFact,
    DependencyCycle,
    InvalidEvent,
    MissingDependency,
    MissingParent,
    ParentCycle,
    UnresolvedTerminalNode,
}

impl ExplainAnalyzeProjectionDiagnosticCodeV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidEvent => "invalid_event",
            Self::ConflictingFact => "conflicting_fact",
            Self::MissingParent => "missing_parent",
            Self::MissingDependency => "missing_dependency",
            Self::ParentCycle => "parent_cycle",
            Self::DependencyCycle => "dependency_cycle",
            Self::UnresolvedTerminalNode => "unresolved_terminal_node",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainAnalyzeProjectionDiagnosticV1 {
    pub code: ExplainAnalyzeProjectionDiagnosticCodeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_node_id: Option<String>,
}

/// Deterministically ordered diagnostics. The small access API keeps the
/// reducer's ordered index private while allowing formatters to iterate it.
#[derive(Clone, Debug, Default)]
pub struct ExplainAnalyzeProjectionDiagnosticsV1 {
    entries: BTreeMap<DiagnosticKey, ExplainAnalyzeProjectionDiagnosticV1>,
    cycle_keys: BTreeSet<DiagnosticKey>,
    next_invalid_occurrence: u64,
}

impl ExplainAnalyzeProjectionDiagnosticsV1 {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ExplainAnalyzeProjectionDiagnosticV1> {
        self.entries.values()
    }

    fn insert(
        &mut self,
        code: ExplainAnalyzeProjectionDiagnosticCodeV1,
        node_id: Option<String>,
        related_node_id: Option<String>,
    ) {
        let occurrence = if code == ExplainAnalyzeProjectionDiagnosticCodeV1::InvalidEvent {
            let occurrence = self.next_invalid_occurrence;
            self.next_invalid_occurrence = self.next_invalid_occurrence.saturating_add(1);
            occurrence
        } else {
            0
        };
        let key = DiagnosticKey {
            code,
            node_id: node_id.clone(),
            related_node_id: related_node_id.clone(),
            occurrence,
        };
        self.entries
            .entry(key.clone())
            .or_insert(ExplainAnalyzeProjectionDiagnosticV1 {
                code,
                node_id,
                related_node_id,
            });
        if matches!(
            code,
            ExplainAnalyzeProjectionDiagnosticCodeV1::ParentCycle
                | ExplainAnalyzeProjectionDiagnosticCodeV1::DependencyCycle
        ) {
            self.cycle_keys.insert(key);
        }
    }

    fn remove(
        &mut self,
        code: ExplainAnalyzeProjectionDiagnosticCodeV1,
        node_id: Option<&str>,
        related_node_id: Option<&str>,
    ) {
        if code == ExplainAnalyzeProjectionDiagnosticCodeV1::InvalidEvent {
            return;
        }
        self.entries.remove(&DiagnosticKey {
            code,
            node_id: node_id.map(str::to_owned),
            related_node_id: related_node_id.map(str::to_owned),
            occurrence: 0,
        });
    }

    fn clear_cycles(&mut self) {
        for key in std::mem::take(&mut self.cycle_keys) {
            self.entries.remove(&key);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DiagnosticKey {
    code: ExplainAnalyzeProjectionDiagnosticCodeV1,
    node_id: Option<String>,
    related_node_id: Option<String>,
    occurrence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplainAnalyzeProjectionApplyResultV1 {
    Inserted {
        node_index: usize,
    },
    Updated {
        node_index: usize,
    },
    Duplicate {
        node_index: Option<usize>,
    },
    Conflicted {
        node_indices: Vec<usize>,
        node_ids: Vec<String>,
    },
    Invalid,
}

#[derive(Clone, Debug)]
struct SeenEvent {
    fingerprint: String,
    node_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TurnScope {
    run_id: String,
    turn_id: String,
    clock_domain_id: String,
}

impl TurnScope {
    fn from_event(event: &ExplainAnalyzeEventV1) -> Self {
        Self {
            run_id: event.run_id.clone(),
            turn_id: event.turn_id.clone(),
            clock_domain_id: event.clock_domain_id.clone(),
        }
    }

    fn from_node(node: &ExplainAnalyzeProjectedNodeV1) -> Self {
        Self {
            run_id: node.run_id.clone(),
            turn_id: node.turn_id.clone(),
            clock_domain_id: node.clock_domain_id.clone(),
        }
    }
}

/// Incremental pure-fact reducer shared by CLI, TUI, and replay consumers.
///
/// `nodes()` is an append-only arena, not a sorted snapshot. Parent/child
/// indexes are maintained as facts arrive, so rendering live facts does not
/// rebuild or clone the graph after every event. Call `finish_ingest()` after
/// a replay batch or a terminal turn fact to calculate cycle diagnostics.
#[derive(Clone, Debug, Default)]
pub struct ExplainAnalyzeGraphV1 {
    nodes: Vec<ExplainAnalyzeProjectedNodeV1>,
    children: Vec<Vec<usize>>,
    node_indices: HashMap<String, usize>,
    seen_events: HashMap<String, SeenEvent>,
    conflicted_node_ids: BTreeSet<String>,
    // Retain conflict provenance even when the incoming usage fact is discarded.
    auxiliary_capture_conflicted: bool,
    duplicate_event_count: usize,
    diagnostics: ExplainAnalyzeProjectionDiagnosticsV1,
    root_indices: BTreeSet<usize>,
    pending_parents: HashMap<String, Vec<usize>>,
    pending_dependencies: HashMap<String, Vec<(usize, usize)>>,
    scope_nodes: HashMap<TurnScope, Vec<usize>>,
    terminal_turn_scopes: HashSet<TurnScope>,
    cycle_root_indices: BTreeSet<usize>,
    cycles_checked: bool,
}

impl ExplainAnalyzeGraphV1 {
    /// Captured terminal usage only. Missing snapshots are not an empty ledger.
    pub fn auxiliary_usage_snapshot(&self) -> crate::ExplainAnalyzeAuxiliaryUsageV1 {
        let (attempts, conflicts) = self.reconcile_auxiliary_attempts();
        if conflicts > 0 || self.auxiliary_capture_conflicted() {
            // The wire has no conflict-coverage field. Do not turn rejected
            // evidence into either zero usage or producer truncation.
            return crate::ExplainAnalyzeAuxiliaryUsageV1 {
                available: false,
                truncated: false,
                attempts: Vec::new(),
            };
        }
        let available = self.nodes.iter().any(|node| {
            node.terminal_observed
                && !node.conflicted
                && node
                    .auxiliary_usage
                    .as_ref()
                    .is_some_and(|usage| usage.available)
        });
        crate::ExplainAnalyzeAuxiliaryUsageV1 {
            available,
            truncated: available && self.auxiliary_usage_truncated(),
            attempts: attempts.into_iter().cloned().collect(),
        }
    }

    /// Deduplicate physical attempts across repeated turn segments.
    pub fn auxiliary_attempts(&self) -> Vec<&crate::ExplainAnalyzeAuxiliaryAttemptV1> {
        self.reconcile_auxiliary_attempts().0
    }

    /// Conflicting physical identities/measurements, not omitted capture rows.
    pub fn auxiliary_usage_conflict_count(&self) -> usize {
        self.reconcile_auxiliary_attempts().1
    }

    /// A conflicted Turn cannot establish capture coverage, even if its
    /// retained version lacks usage. This is not an attempt-identity count.
    pub fn auxiliary_capture_conflicted(&self) -> bool {
        self.auxiliary_capture_conflicted
            || self.nodes.iter().any(|node| {
                node.conflicted
                    && (node.kind == ExplainAnalyzeNodeKindV1::Turn
                        || node.auxiliary_usage.is_some())
            })
    }

    fn reconcile_auxiliary_attempts(
        &self,
    ) -> (Vec<&crate::ExplainAnalyzeAuxiliaryAttemptV1>, usize) {
        use crate::{
            ExplainAnalyzeAuxiliaryAttemptV1 as Attempt,
            ExplainAnalyzeAuxiliaryUsageStatusV1 as Status,
        };
        let buckets = |attempt: &Attempt| {
            attempt.usage.as_ref().map_or([None; 4], |usage| {
                [
                    usage.fresh_input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens,
                ]
            })
        };
        fn identity(attempt: &Attempt) -> [&str; 5] {
            // Compare below without allocating or minting a replacement fact.
            [
                attempt.provider.as_str(),
                attempt.offering_id.as_str(),
                attempt.model_name.as_str(),
                attempt.purpose.as_str(),
                attempt.operation_id.as_str(),
            ]
        }
        let rank = |status| match status {
            Status::Unavailable => 0,
            Status::ProviderPartial => 1,
            Status::ProviderExact => 2,
        };
        // Consensus buckets are comparison evidence only, never merged into
        // the selected record. Remember weaker observations even when a later
        // exact record omits that bucket, so contradictions cannot disappear.
        let mut attempts = BTreeMap::new();
        for attempt in self
            .nodes
            .iter()
            .filter(|n| n.terminal_observed && !n.conflicted)
            .filter_map(|n| n.auxiliary_usage.as_ref())
            .flat_map(|u| &u.attempts)
        {
            let (selected, known, conflicted) =
                attempts
                    .entry(&attempt.attempt_id)
                    .or_insert((attempt, buckets(attempt), false));
            *conflicted |= identity(selected) != identity(attempt);
            for (previous, current) in known.iter_mut().zip(buckets(attempt)) {
                if let Some(current) = current {
                    if let Some(previous) = previous {
                        *conflicted |= *previous != current;
                    } else {
                        *previous = Some(current);
                    }
                }
            }
            // Stable tie-break between compatible original records. Never
            // add buckets or promote a partial-only value to exact evidence.
            let key = |a: &Attempt| (rank(a.usage_status), a.usage.is_some(), buckets(a));
            if key(attempt) > key(selected) {
                *selected = attempt;
            }
        }
        let conflicts = attempts
            .values()
            .filter(|(_, _, conflict)| *conflict)
            .count();
        let retained = attempts
            .into_values()
            .filter_map(|(attempt, _, conflict)| (!conflict).then_some(attempt))
            .collect();
        (retained, conflicts)
    }
    pub fn auxiliary_usage_unavailable(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| n.auxiliary_usage.as_ref().is_some_and(|u| !u.available))
    }

    pub fn auxiliary_usage_truncated(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| n.auxiliary_usage.as_ref().is_some_and(|u| u.truncated))
    }

    pub fn nodes(&self) -> &[ExplainAnalyzeProjectedNodeV1] {
        &self.nodes
    }

    /// Return roots in first-seen order. Nodes with unresolved parents are
    /// visible as roots; a detected parent-cycle contributes one or more
    /// diagnostic anchors so malformed components remain renderable.
    pub fn roots(&self) -> impl Iterator<Item = usize> + '_ {
        let known_roots = self.root_indices.union(&self.cycle_root_indices).copied();
        let fallback = (!self.nodes.is_empty()
            && self.root_indices.is_empty()
            && self.cycle_root_indices.is_empty())
        .then_some(0);
        known_roots.chain(fallback)
    }

    pub fn children(&self, node_index: usize) -> &[usize] {
        self.children
            .get(node_index)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn diagnostics(&self) -> &ExplainAnalyzeProjectionDiagnosticsV1 {
        &self.diagnostics
    }

    pub fn integrity(&self) -> ExplainAnalyzeGraphIntegrityV1 {
        if self.cycles_checked && self.diagnostics.is_empty() {
            ExplainAnalyzeGraphIntegrityV1::Consistent
        } else {
            ExplainAnalyzeGraphIntegrityV1::Unknown
        }
    }

    pub fn duplicate_event_count(&self) -> usize {
        self.duplicate_event_count
    }

    /// Known measurement boundaries omitted by the producer. This is coverage
    /// metadata, separate from structural graph integrity.
    pub fn coverage_gaps(&self) -> Vec<crate::ExplainAnalyzeCoverageGapV1> {
        let mut coverage_gaps = self
            .nodes
            .iter()
            .filter(|node| node.kind == ExplainAnalyzeNodeKindV1::Turn && node.terminal_observed)
            .flat_map(|node| node.coverage_gaps.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        coverage_gaps.sort_by_key(|gap| gap.as_str());
        coverage_gaps
    }

    pub fn conflicted_node_ids(&self) -> impl ExactSizeIterator<Item = &str> {
        self.conflicted_node_ids.iter().map(String::as_str)
    }

    /// Apply one validated or untrusted typed fact. Invalid facts and
    /// conflicting event identities are retained as diagnostics, never used
    /// to rewrite previously accepted node metadata.
    pub fn apply(&mut self, event: ExplainAnalyzeEventV1) -> ExplainAnalyzeProjectionApplyResultV1 {
        if !event.is_valid() {
            self.diagnostics.insert(
                ExplainAnalyzeProjectionDiagnosticCodeV1::InvalidEvent,
                None,
                None,
            );
            return ExplainAnalyzeProjectionApplyResultV1::Invalid;
        }

        let fingerprint = event_fingerprint(&event);
        if let Some(seen) = self.seen_events.get(&event.event_id).cloned() {
            self.duplicate_event_count = self.duplicate_event_count.saturating_add(1);
            if seen.fingerprint == fingerprint {
                return ExplainAnalyzeProjectionApplyResultV1::Duplicate {
                    node_index: self.node_indices.get(&seen.node_id).copied(),
                };
            }

            self.auxiliary_capture_conflicted |=
                event.kind == ExplainAnalyzeNodeKindV1::Turn || event.auxiliary_usage.is_some();
            let mut node_ids = BTreeSet::new();
            node_ids.insert(seen.node_id);
            node_ids.insert(event.node_id.clone());
            let mut node_indices = Vec::new();
            for node_id in &node_ids {
                if let Some(index) = self.mark_conflicted_node_id(node_id) {
                    node_indices.push(index);
                }
            }
            return ExplainAnalyzeProjectionApplyResultV1::Conflicted {
                node_indices,
                node_ids: node_ids.into_iter().collect(),
            };
        }
        self.seen_events.insert(
            event.event_id.clone(),
            SeenEvent {
                fingerprint,
                node_id: event.node_id.clone(),
            },
        );

        let (node_index, inserted) = if let Some(index) = self.node_indices.get(&event.node_id) {
            (*index, false)
        } else {
            (self.insert_node(&event), true)
        };

        let mut conflict = self.conflicted_node_ids.contains(&event.node_id);
        if !inserted {
            conflict |= self.merge_event(node_index, &event);
        }
        if conflict {
            self.auxiliary_capture_conflicted |=
                event.kind == ExplainAnalyzeNodeKindV1::Turn || event.auxiliary_usage.is_some();
            self.mark_conflicted_node_id(&event.node_id);
        }

        self.refresh_unresolved_terminal(node_index);
        // Scope closure follows the merged node. A conflicting incoming fact
        // can disagree about kind, but once the retained node is a terminal
        // Turn its scope is still the evidence boundary for open siblings.
        let merged_scope = self.nodes.get(node_index).and_then(|merged_node| {
            (merged_node.kind == ExplainAnalyzeNodeKindV1::Turn && merged_node.terminal_observed)
                .then(|| TurnScope::from_node(merged_node))
        });
        if let Some(scope) = merged_scope {
            self.mark_terminal_turn(scope);
        }

        if inserted {
            // A newly observed node can resolve edges waiting for its id and
            // can introduce cycles. Existing cycle anchors remain valid as
            // facts are append-only; finish_ingest refreshes the diagnostics.
            self.cycles_checked = false;
        }

        if conflict {
            ExplainAnalyzeProjectionApplyResultV1::Conflicted {
                node_indices: vec![node_index],
                node_ids: vec![event.node_id],
            }
        } else if inserted {
            ExplainAnalyzeProjectionApplyResultV1::Inserted { node_index }
        } else {
            ExplainAnalyzeProjectionApplyResultV1::Updated { node_index }
        }
    }

    /// Complete graph-wide checks after a durable replay batch or a completed
    /// turn. This is intentionally explicit: cycle analysis is linear in the
    /// facts observed so far and is not repeated for every live event.
    pub fn finish_ingest(&mut self) {
        self.diagnostics.clear_cycles();
        self.cycle_root_indices.clear();
        for (code, edges) in [
            (
                ExplainAnalyzeProjectionDiagnosticCodeV1::ParentCycle,
                CycleEdges::Parents,
            ),
            (
                ExplainAnalyzeProjectionDiagnosticCodeV1::DependencyCycle,
                CycleEdges::Dependencies,
            ),
        ] {
            for (source, target) in self.find_cycles(edges) {
                let source_id = self.nodes[source].node_id.clone();
                let target_id = self.nodes[target].node_id.clone();
                self.diagnostics
                    .insert(code, Some(source_id.clone()), Some(target_id));
                if code == ExplainAnalyzeProjectionDiagnosticCodeV1::ParentCycle {
                    self.cycle_root_indices.insert(source);
                }
            }
        }
        self.cycles_checked = true;
    }

    /// Maximum observed leaf concurrency. Clock domains are evaluated
    /// independently and the maximum is returned; values from distinct clocks
    /// are never added together. `None` means the graph is incomplete or its
    /// integrity is unknown.
    pub fn max_concurrency(&self) -> Option<usize> {
        if self.integrity() != ExplainAnalyzeGraphIntegrityV1::Consistent
            || self.nodes.iter().any(|node| !node.terminal_observed)
        {
            return None;
        }

        let mut parent_ids = HashSet::new();
        for node in &self.nodes {
            if let Some(parent_id) = &node.parent_node_id {
                parent_ids.insert((node.clock_domain_id.as_str(), parent_id.as_str()));
            }
        }

        let mut points_by_clock: HashMap<&str, Vec<(u64, i8)>> = HashMap::new();
        for node in &self.nodes {
            if matches!(
                node.kind,
                ExplainAnalyzeNodeKindV1::Admission | ExplainAnalyzeNodeKindV1::Wait
            ) {
                continue;
            }
            if parent_ids.contains(&(node.clock_domain_id.as_str(), node.node_id.as_str())) {
                continue;
            }
            let Some(end) = node.end_elapsed_ms else {
                continue;
            };
            if end <= node.start_elapsed_ms {
                continue;
            }
            let points = points_by_clock
                .entry(node.clock_domain_id.as_str())
                .or_default();
            points.push((node.start_elapsed_ms, 1));
            points.push((end, -1));
        }

        let mut maximum = 0usize;
        let mut saw_interval = false;
        for points in points_by_clock.values_mut() {
            saw_interval = true;
            points.sort_unstable(); // End (-1) precedes start (+1) at equal time.
            let mut active = 0usize;
            for (_, delta) in points {
                if *delta < 0 {
                    active = active.saturating_sub(1);
                } else {
                    active = active.saturating_add(1);
                }
                maximum = maximum.max(active);
            }
        }
        saw_interval.then_some(maximum)
    }

    fn insert_node(&mut self, event: &ExplainAnalyzeEventV1) -> usize {
        let terminal = event.transition == ExplainAnalyzeTransitionV1::Finished;
        let start_elapsed_ms = if terminal {
            // `is_valid` guarantees the finish carries its original start.
            event.start_elapsed_ms.unwrap_or(event.elapsed_ms)
        } else {
            event.elapsed_ms
        };
        let node_index = self.nodes.len();
        let node = ExplainAnalyzeProjectedNodeV1 {
            node_id: event.node_id.clone(),
            run_id: event.run_id.clone(),
            turn_id: event.turn_id.clone(),
            clock_domain_id: event.clock_domain_id.clone(),
            kind: event.kind,
            label: event.label.clone(),
            parent_node_id: event.parent_node_id.clone(),
            parent_index: None,
            dependency_node_ids: event.dependency_node_ids.clone(),
            dependency_indices: vec![None; event.dependency_node_ids.len()],
            round_index: event.round_index,
            attempt_index: event.attempt_index,
            start_elapsed_ms,
            end_elapsed_ms: terminal.then_some(event.elapsed_ms),
            duration_ms: event.duration_ms,
            outcome: event.outcome,
            usage: event.usage.clone(),
            context: event.context.clone(),
            auxiliary_usage: event.auxiliary_usage.clone(),
            coverage_gaps: event.coverage_gaps.clone(),
            start_observed: event.transition == ExplainAnalyzeTransitionV1::Started,
            terminal_observed: terminal,
            conflicted: self.conflicted_node_ids.contains(&event.node_id),
        };
        self.nodes.push(node);
        self.children.push(Vec::new());
        self.node_indices.insert(event.node_id.clone(), node_index);
        self.scope_nodes
            .entry(TurnScope::from_event(event))
            .or_default()
            .push(node_index);

        // Resolve older nodes which referred to this node before its fact was
        // received. The reverse indexes make this proportional to the number
        // of waiting references rather than the whole graph.
        let mut resolved_waiting_children = false;
        if let Some(waiting_children) = self.pending_parents.remove(&event.node_id) {
            resolved_waiting_children = !waiting_children.is_empty();
            for child_index in waiting_children {
                self.nodes[child_index].parent_index = Some(node_index);
                self.root_indices.remove(&child_index);
                self.children[node_index].push(child_index);
                self.diagnostics.remove(
                    ExplainAnalyzeProjectionDiagnosticCodeV1::MissingParent,
                    Some(&self.nodes[child_index].node_id),
                    Some(&event.node_id),
                );
            }
        }
        if let Some(waiting_dependencies) = self.pending_dependencies.remove(&event.node_id) {
            for (dependent_index, dependency_position) in waiting_dependencies {
                self.nodes[dependent_index].dependency_indices[dependency_position] =
                    Some(node_index);
                self.diagnostics.remove(
                    ExplainAnalyzeProjectionDiagnosticCodeV1::MissingDependency,
                    Some(&self.nodes[dependent_index].node_id),
                    Some(&event.node_id),
                );
            }
        }

        if let Some(parent_id) = event.parent_node_id.as_deref() {
            if let Some(parent_index) = self.node_indices.get(parent_id).copied() {
                self.nodes[node_index].parent_index = Some(parent_index);
                self.children[parent_index].push(node_index);
            } else {
                self.root_indices.insert(node_index);
                self.pending_parents
                    .entry(parent_id.to_owned())
                    .or_default()
                    .push(node_index);
                self.diagnostics.insert(
                    ExplainAnalyzeProjectionDiagnosticCodeV1::MissingParent,
                    Some(event.node_id.clone()),
                    Some(parent_id.to_owned()),
                );
            }
        } else {
            self.root_indices.insert(node_index);
        }
        for (position, dependency_id) in event.dependency_node_ids.iter().enumerate() {
            if let Some(dependency_index) = self.node_indices.get(dependency_id).copied() {
                self.nodes[node_index].dependency_indices[position] = Some(dependency_index);
            } else {
                self.pending_dependencies
                    .entry(dependency_id.clone())
                    .or_default()
                    .push((node_index, position));
                self.diagnostics.insert(
                    ExplainAnalyzeProjectionDiagnosticCodeV1::MissingDependency,
                    Some(event.node_id.clone()),
                    Some(dependency_id.clone()),
                );
            }
        }

        if self.conflicted_node_ids.contains(&event.node_id) {
            self.nodes[node_index].conflicted = true;
        }
        if resolved_waiting_children
            && let Some(cycle_anchor) = self.find_parent_cycle_anchor_from(node_index)
        {
            self.cycle_root_indices.insert(cycle_anchor);
        }
        node_index
    }

    /// Detect a newly closed parent cycle without walking the graph for every
    /// ordinary insertion. A cycle can be closed only when a previously
    /// unresolved parent reference is linked to its node; that path is walked
    /// once at the point of resolution.
    fn find_parent_cycle_anchor_from(&self, start: usize) -> Option<usize> {
        let mut positions: HashMap<usize, usize> = HashMap::new();
        let mut path: Vec<usize> = Vec::new();
        let mut current = Some(start);
        while let Some(index) = current {
            if let Some(position) = positions.get(&index).copied() {
                return path.get(position).copied();
            }
            positions.insert(index, path.len());
            path.push(index);
            current = self.nodes.get(index)?.parent_index;
        }
        None
    }

    fn merge_event(&mut self, node_index: usize, event: &ExplainAnalyzeEventV1) -> bool {
        let mut conflict = false;
        {
            let node = &self.nodes[node_index];
            if node.run_id != event.run_id
                || node.turn_id != event.turn_id
                || node.clock_domain_id != event.clock_domain_id
                || node.kind != event.kind
                || node.label != event.label
                || node.parent_node_id != event.parent_node_id
                || node.round_index != event.round_index
                || node.attempt_index != event.attempt_index
                || node.dependency_node_ids != event.dependency_node_ids
            {
                conflict = true;
            }
        }

        let node = &mut self.nodes[node_index];
        match event.transition {
            ExplainAnalyzeTransitionV1::Started => {
                if (node.start_observed || node.terminal_observed)
                    && node.start_elapsed_ms != event.elapsed_ms
                {
                    conflict = true;
                } else if !node.start_observed {
                    node.start_elapsed_ms = event.elapsed_ms;
                    node.start_observed = true;
                }
            }
            ExplainAnalyzeTransitionV1::Finished if node.terminal_observed => {
                if node.start_elapsed_ms != event.start_elapsed_ms.unwrap_or(event.elapsed_ms)
                    || node.end_elapsed_ms != Some(event.elapsed_ms)
                    || node.duration_ms != event.duration_ms
                    || node.outcome != event.outcome
                    || node.usage != event.usage
                    || node.auxiliary_usage != event.auxiliary_usage
                    || node.context != event.context
                    || node.coverage_gaps != event.coverage_gaps
                {
                    conflict = true;
                }
            }
            ExplainAnalyzeTransitionV1::Finished => {
                let event_start = event.start_elapsed_ms.unwrap_or(event.elapsed_ms);
                if node.start_observed && node.start_elapsed_ms != event_start {
                    conflict = true;
                } else if !node.start_observed {
                    node.start_elapsed_ms = event_start;
                }
                node.end_elapsed_ms = Some(event.elapsed_ms);
                node.duration_ms = event.duration_ms;
                node.outcome = event.outcome;
                node.usage = event.usage.clone();
                node.context = event.context.clone();
                node.auxiliary_usage = event.auxiliary_usage.clone();
                node.coverage_gaps = event.coverage_gaps.clone();
                node.terminal_observed = true;
            }
        }
        if conflict {
            node.conflicted = true;
        }
        conflict
    }

    fn mark_conflicted_node_id(&mut self, node_id: &str) -> Option<usize> {
        self.conflicted_node_ids.insert(node_id.to_owned());
        self.diagnostics.insert(
            ExplainAnalyzeProjectionDiagnosticCodeV1::ConflictingFact,
            Some(node_id.to_owned()),
            None,
        );
        let index = self.node_indices.get(node_id).copied();
        if let Some(index) = index {
            self.nodes[index].conflicted = true;
        }
        index
    }

    fn refresh_unresolved_terminal(&mut self, node_index: usize) {
        let node = &self.nodes[node_index];
        let scope = TurnScope::from_node(node);
        let node_id = node.node_id.clone();
        if node.terminal_observed {
            self.diagnostics.remove(
                ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode,
                Some(&node_id),
                None,
            );
        } else if self.terminal_turn_scopes.contains(&scope) {
            self.diagnostics.insert(
                ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode,
                Some(node_id),
                None,
            );
        }
    }

    fn mark_terminal_turn(&mut self, scope: TurnScope) {
        if !self.terminal_turn_scopes.insert(scope.clone()) {
            return;
        }
        let unresolved: Vec<usize> = self
            .scope_nodes
            .get(&scope)
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| !self.nodes[*index].terminal_observed)
            .collect();
        for index in unresolved {
            let node_id = self.nodes[index].node_id.clone();
            self.diagnostics.insert(
                ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode,
                Some(node_id),
                None,
            );
        }
    }

    fn find_cycles(&self, edges: CycleEdges) -> Vec<(usize, usize)> {
        let mut colors = vec![0u8; self.nodes.len()];
        let mut cycles = Vec::new();
        let mut roots: Vec<usize> = (0..self.nodes.len()).collect();
        roots.sort_unstable_by(|left, right| {
            self.nodes[*left].node_id.cmp(&self.nodes[*right].node_id)
        });
        for root in roots {
            if colors[root] != 0 {
                continue;
            }
            colors[root] = 1;
            let mut stack = vec![CycleFrame {
                node_index: root,
                next_edge: 0,
            }];
            while let Some(frame) = stack.last_mut() {
                let edge_count = edge_count(&self.nodes[frame.node_index], edges);
                if frame.next_edge >= edge_count {
                    colors[frame.node_index] = 2;
                    stack.pop();
                    continue;
                }
                let edge_position = frame.next_edge;
                frame.next_edge += 1;
                let source = frame.node_index;
                let Some(target) = edge_at(&self.nodes[source], edges, edge_position) else {
                    continue;
                };
                match colors[target] {
                    1 => cycles.push((source, target)),
                    0 => {
                        colors[target] = 1;
                        stack.push(CycleFrame {
                            node_index: target,
                            next_edge: 0,
                        });
                    }
                    _ => {}
                }
            }
        }
        cycles
    }
}

#[derive(Clone, Copy)]
enum CycleEdges {
    Parents,
    Dependencies,
}

struct CycleFrame {
    node_index: usize,
    next_edge: usize,
}

fn edge_count(node: &ExplainAnalyzeProjectedNodeV1, edges: CycleEdges) -> usize {
    match edges {
        CycleEdges::Parents => usize::from(node.parent_index.is_some()),
        // Include unresolved slots and skip them in edge_at. Keeping the
        // frame cursor aligned with the underlying vector makes each
        // dependency edge visit O(1), even for high-degree nodes.
        CycleEdges::Dependencies => node.dependency_indices.len(),
    }
}

fn edge_at(
    node: &ExplainAnalyzeProjectedNodeV1,
    edges: CycleEdges,
    edge_position: usize,
) -> Option<usize> {
    match edges {
        CycleEdges::Parents => (edge_position == 0).then_some(node.parent_index).flatten(),
        CycleEdges::Dependencies => node
            .dependency_indices
            .get(edge_position)
            .copied()
            .flatten(),
    }
}

fn event_fingerprint(event: &ExplainAnalyzeEventV1) -> String {
    // Match the SDK's stable JSON fact identity: the wire discriminator is
    // part of the fact, transport index is not, empty dependency lists are
    // omitted, and object keys are sorted recursively. The shared Rust wire
    // decoder removes `index` before this typed event reaches the reducer.
    let mut canonical = serde_json::to_value(event)
        .expect("Explain Analyze V1 event contains only JSON-serializable fields");
    let object = canonical
        .as_object_mut()
        .expect("Explain Analyze V1 event serializes as an object");
    object.insert(
        "type".to_owned(),
        serde_json::Value::String(EXPLAIN_ANALYZE_EVENT_TYPE.to_owned()),
    );
    object.remove("index");
    if object
        .get("dependency_node_ids")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        object.remove("dependency_node_ids");
    }
    stable_json(&canonical)
}

fn stable_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items.iter().map(stable_json).collect::<Vec<_>>().join(",")
        ),
        serde_json::Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort_unstable();
            let entries = keys
                .into_iter()
                .map(|key| {
                    let encoded_key = serde_json::to_string(key)
                        .expect("JSON object keys are always serializable");
                    let nested = object
                        .get(key.as_str())
                        .expect("a key came from this JSON object");
                    format!("{encoded_key}:{}", stable_json(nested))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{entries}}}")
        }
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ExplainAnalyzeContextAssemblyBasisV1, ExplainAnalyzeContextAssemblyV1,
        ExplainAnalyzeContextBudgetBasisV1, ExplainAnalyzeContextBudgetV1,
        ExplainAnalyzeContextSourceKindV1, ExplainAnalyzeContextSourceV1,
    };

    fn started(
        node_id: &str,
        kind: ExplainAnalyzeNodeKindV1,
        parent_node_id: Option<&str>,
        clock_domain_id: &str,
        elapsed_ms: u64,
    ) -> ExplainAnalyzeEventV1 {
        ExplainAnalyzeEventV1 {
            auxiliary_usage: None,
            schema_version: crate::EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: format!("{node_id}/started"),
            run_id: "run-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            node_id: node_id.to_owned(),
            parent_node_id: parent_node_id.map(str::to_owned),
            dependency_node_ids: Vec::new(),
            producer_id: "worker-1".to_owned(),
            clock_domain_id: clock_domain_id.to_owned(),
            kind,
            round_index: (kind == ExplainAnalyzeNodeKindV1::ModelRound
                || kind == ExplainAnalyzeNodeKindV1::ProviderAttempt)
                .then_some(0),
            attempt_index: (kind == ExplainAnalyzeNodeKindV1::ProviderAttempt).then_some(0),
            label: format!("{node_id} label"),
            transition: ExplainAnalyzeTransitionV1::Started,
            elapsed_ms,
            start_elapsed_ms: None,
            duration_ms: None,
            outcome: None,
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        }
    }

    fn finished(
        mut event: ExplainAnalyzeEventV1,
        start_elapsed_ms: u64,
        end_elapsed_ms: u64,
    ) -> ExplainAnalyzeEventV1 {
        event.event_id = format!("{}/finished", event.node_id);
        event.transition = ExplainAnalyzeTransitionV1::Finished;
        event.elapsed_ms = end_elapsed_ms;
        event.start_elapsed_ms = Some(start_elapsed_ms);
        event.duration_ms = Some(end_elapsed_ms - start_elapsed_ms);
        event.outcome = Some(ExplainAnalyzeOutcomeV1::Succeeded);
        event
    }

    #[test]
    fn auxiliary_snapshots_upgrade_status_without_double_counting_attempts() {
        use crate::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1, ExplainAnalyzeTokenUsageV1, ExplainAnalyzeUsageBasisV1,
        };
        let mut graph = ExplainAnalyzeGraphV1::default();
        for (index, status) in [
            ExplainAnalyzeAuxiliaryUsageStatusV1::Unavailable,
            ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderPartial,
            ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
        ]
        .into_iter()
        .enumerate()
        {
            let mut event = finished(
                started(
                    &format!("segment-{index}"),
                    ExplainAnalyzeNodeKindV1::Turn,
                    None,
                    "clock",
                    0,
                ),
                0,
                10,
            );
            event.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: vec![ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: "aux-1".into(),
                    provider: "typesafe".into(),
                    offering_id: "jev-1".into(),
                    model_name: "jev1".into(),
                    purpose: "verification_judge".into(),
                    operation_id: "verification_judge".into(),
                    usage_status: status,
                    usage: (status == ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact)
                        .then_some(ExplainAnalyzeTokenUsageV1 {
                            basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
                            fresh_input_tokens: Some(100),
                            output_tokens: Some(0),
                            cache_read_tokens: None,
                            cache_creation_tokens: None,
                        }),
                }],
            }));
            graph.apply(event);
            assert_eq!(graph.auxiliary_attempts().len(), 1);
            assert_eq!(graph.auxiliary_attempts()[0].usage_status, status);
            let snapshot = graph.auxiliary_usage_snapshot();
            assert!(snapshot.available);
            assert_eq!(snapshot.attempts.len(), 1);
            assert_eq!(snapshot.attempts[0].usage_status, status);
        }
        assert_eq!(
            graph.auxiliary_attempts()[0]
                .usage
                .as_ref()
                .unwrap()
                .fresh_input_tokens,
            Some(100)
        );
    }

    #[test]
    fn auxiliary_conflicts_are_sticky_and_permutation_independent() {
        use crate::{
            ExplainAnalyzeAuxiliaryAttemptV1 as Attempt,
            ExplainAnalyzeAuxiliaryUsageStatusV1 as Status, ExplainAnalyzeAuxiliaryUsageV1,
            ExplainAnalyzeUsageBasisV1 as Basis,
        };
        fn attempt(status: Status, counts: [Option<u64>; 4]) -> Attempt {
            Attempt {
                attempt_id: "physical-1".into(),
                provider: "provider".into(),
                offering_id: "offering".into(),
                model_name: "model".into(),
                purpose: "verification_judge".into(),
                operation_id: "request_judgment".into(),
                usage_status: status,
                usage: (status != Status::Unavailable).then_some(ExplainAnalyzeTokenUsageV1 {
                    basis: if status == Status::ProviderExact {
                        Basis::ProviderExact
                    } else {
                        Basis::ProviderPartial
                    },
                    fresh_input_tokens: counts[0],
                    output_tokens: counts[1],
                    cache_read_tokens: counts[2],
                    cache_creation_tokens: counts[3],
                }),
            }
        }
        fn graph(records: &[Attempt], order: [usize; 3]) -> ExplainAnalyzeGraphV1 {
            let mut graph = ExplainAnalyzeGraphV1::default();
            for index in order {
                let mut event = finished(
                    started(
                        &format!("segment-{index}"),
                        ExplainAnalyzeNodeKindV1::Turn,
                        None,
                        "clock",
                        0,
                    ),
                    0,
                    10,
                );
                event.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
                    available: true,
                    truncated: false,
                    attempts: vec![records[index].clone()],
                }));
                assert!(event.is_valid());
                graph.apply(event.clone());
                graph.apply(event); // Replay cannot double-count or revive conflicts.
            }
            graph
        }
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let unknown = attempt(Status::Unavailable, [None; 4]);
        let partial = attempt(Status::ProviderPartial, [Some(10), None, None, None]);
        let exact = attempt(Status::ProviderExact, [Some(10), Some(0), None, None]);
        for order in orders {
            let graph = graph(&[unknown.clone(), partial.clone(), exact.clone()], order);
            assert_eq!(graph.auxiliary_attempts(), vec![&exact]);
            assert_eq!(graph.auxiliary_usage_conflict_count(), 0);
            let snapshot = graph.auxiliary_usage_snapshot();
            assert!(snapshot.available && !snapshot.truncated);
            assert_eq!(
                snapshot.attempts[0].usage.as_ref().unwrap().output_tokens,
                Some(0)
            );
            assert_eq!(
                snapshot.attempts[0]
                    .usage
                    .as_ref()
                    .unwrap()
                    .cache_read_tokens,
                None
            );
        }
        // A same-rank complementary record is not fused with another fact.
        let complementary = attempt(Status::ProviderPartial, [None, Some(0), None, None]);
        for order in orders {
            let graph = graph(
                &[partial.clone(), complementary.clone(), unknown.clone()],
                order,
            );
            assert_eq!(graph.auxiliary_attempts(), vec![&partial]);
        }
        let mut conflicts = Vec::new();
        for field in 0..5 {
            let mut other = exact.clone();
            let identity = match field {
                0 => &mut other.provider,
                1 => &mut other.offering_id,
                2 => &mut other.model_name,
                3 => &mut other.purpose,
                _ => &mut other.operation_id,
            };
            *identity = "other".into();
            conflicts.push([exact.clone(), other, exact.clone()]);
        }
        for bucket in 0..4 {
            let mut ten = [None; 4];
            ten[bucket] = Some(10);
            let mut twenty = [None; 4];
            twenty[bucket] = Some(20);
            for (left, right) in [
                (Status::ProviderPartial, Status::ProviderPartial),
                (Status::ProviderExact, Status::ProviderExact),
                (Status::ProviderPartial, Status::ProviderExact),
            ] {
                conflicts.push([attempt(left, ten), attempt(right, twenty), unknown.clone()]);
            }
            // The higher-ranked record cannot erase weaker known evidence.
            let mut omitted = [None; 4];
            omitted[(bucket + 1) % 4] = Some(0);
            conflicts.push([
                attempt(Status::ProviderPartial, ten),
                attempt(Status::ProviderExact, omitted),
                attempt(Status::ProviderPartial, twenty),
            ]);
        }
        for records in conflicts {
            for order in orders {
                let graph = graph(&records, order);
                assert!(graph.auxiliary_attempts().is_empty());
                assert_eq!(graph.auxiliary_usage_conflict_count(), 1);
                let snapshot = graph.auxiliary_usage_snapshot();
                assert!(!snapshot.available && !snapshot.truncated);
                assert!(snapshot.attempts.is_empty());
                assert!(snapshot.is_valid());
            }
        }
        let mut retry = exact.clone();
        retry.attempt_id = "physical-2".into();
        let healthy = graph(&[exact.clone(), retry.clone(), exact.clone()], [0, 1, 2]);
        assert_eq!(healthy.auxiliary_attempts().len(), 2);
        let mut corrupt = exact.clone();
        corrupt.provider = "other".into();
        let mixed = graph(&[exact, corrupt, retry], [0, 1, 2]);
        assert_eq!(mixed.auxiliary_attempts().len(), 1);
        assert!(!mixed.auxiliary_usage_snapshot().available);
        assert!(!mixed.auxiliary_usage_truncated());
    }

    #[test]
    fn conflicted_turn_usage_cannot_be_hidden_by_an_independent_snapshot() {
        use crate::{
            ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
            ExplainAnalyzeAuxiliaryUsageV1, ExplainAnalyzeUsageBasisV1,
        };
        let make = |node: &str, count| {
            let mut event = finished(
                started(node, ExplainAnalyzeNodeKindV1::Turn, None, "clock", 0),
                0,
                10,
            );
            event.auxiliary_usage = Some(Box::new(ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: vec![ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: "same-attempt".into(),
                    provider: "provider".into(),
                    offering_id: "offering".into(),
                    model_name: "model".into(),
                    purpose: "verification_judge".into(),
                    operation_id: "request_judgment".into(),
                    usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
                    usage: Some(ExplainAnalyzeTokenUsageV1 {
                        basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
                        fresh_input_tokens: Some(count),
                        output_tokens: Some(0),
                        cache_read_tokens: None,
                        cache_creation_tokens: None,
                    }),
                }],
            }));
            event
        };
        for retained_has_usage in [false, true] {
            for same_event_id in [false, true] {
                let mut original = make("a", 10);
                if !retained_has_usage {
                    original.auxiliary_usage = None;
                }
                let mut conflict = make("a", 30);
                if !same_event_id {
                    conflict.event_id = "another-terminal".into();
                }
                let records = [original, conflict, make("b", 20)];
                for order in [
                    [0, 1, 2],
                    [0, 2, 1],
                    [1, 0, 2],
                    [1, 2, 0],
                    [2, 0, 1],
                    [2, 1, 0],
                ] {
                    let mut graph = ExplainAnalyzeGraphV1::default();
                    for index in order {
                        graph.apply(records[index].clone());
                        graph.apply(records[index].clone());
                    }
                    assert!(graph.auxiliary_capture_conflicted());
                    assert_eq!(graph.auxiliary_usage_conflict_count(), 0);
                    assert!(!graph.auxiliary_usage_truncated());
                    let snapshot = graph.auxiliary_usage_snapshot();
                    assert!(
                        !snapshot.available && !snapshot.truncated && snapshot.attempts.is_empty()
                    );
                }
            }
        }
    }

    #[test]
    fn absent_auxiliary_capture_is_not_zero_usage() {
        assert!(
            !ExplainAnalyzeGraphV1::default()
                .auxiliary_usage_snapshot()
                .available
        );
    }

    fn is_diagnostic(
        graph: &ExplainAnalyzeGraphV1,
        code: ExplainAnalyzeProjectionDiagnosticCodeV1,
        node_id: Option<&str>,
        related_node_id: Option<&str>,
    ) -> bool {
        graph.diagnostics().iter().any(|diagnostic| {
            diagnostic.code == code
                && diagnostic.node_id.as_deref() == node_id
                && diagnostic.related_node_id.as_deref() == related_node_id
        })
    }

    #[test]
    fn fact_fingerprint_matches_sdk_canonical_json_for_live_and_replay_wires() {
        let event = started(
            "node",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-1",
            5,
        );
        let mut live_wire = serde_json::to_value(&event).unwrap();
        live_wire["type"] = serde_json::json!(EXPLAIN_ANALYZE_EVENT_TYPE);
        assert!(live_wire.get("dependency_node_ids").is_none());

        let mut replay_wire = live_wire.clone();
        replay_wire["index"] = serde_json::json!(7);
        replay_wire["dependency_node_ids"] = serde_json::json!([]);
        let live_fact = crate::decode_explain_analyze_wire(&live_wire).unwrap();
        let replay_fact = crate::decode_explain_analyze_wire(&replay_wire).unwrap();

        assert_eq!(live_fact, replay_fact);
        assert_eq!(
            event_fingerprint(&live_fact),
            event_fingerprint(&replay_fact)
        );
        // Shared with the SDK fixture: type is part of the fact, transport
        // index is not, empty dependencies are omitted, and keys are sorted.
        const SDK_CANONICAL_FACT: &str = r#"{"clock_domain_id":"clock-1","elapsed_ms":5,"event_id":"node/started","kind":"tool_call","label":"node label","node_id":"node","producer_id":"worker-1","run_id":"run-1","schema_version":1,"transition":"started","turn_id":"turn-1","type":"explain_analyze"}"#;
        assert_eq!(event_fingerprint(&live_fact), SDK_CANONICAL_FACT);
    }

    #[test]
    fn terminal_fact_reconstructs_a_missed_start_and_keeps_context_metrics() {
        let mut preparation = started(
            "prep-1",
            ExplainAnalyzeNodeKindV1::Preparation,
            Some("turn-node"),
            "clock-1",
            12,
        );
        preparation.context = None;
        let mut finished_preparation = finished(preparation, 12, 33);
        finished_preparation.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: Some(ExplainAnalyzeContextBudgetV1 {
                basis: ExplainAnalyzeContextBudgetBasisV1::PreProviderEstimate,
                estimated_input_tokens: 120,
                estimated_system_tokens: 40,
                tool_schema_tokens: 18,
                requested_output_tokens: 80,
                reserved_protocol_tokens: 12,
                effective_input_limit_tokens: 800,
                model_context_limit_tokens: 1_000,
                visible_tool_count: 3,
            }),
            assembly: None,
        });

        let mut graph = ExplainAnalyzeGraphV1::default();
        assert!(matches!(
            graph.apply(finished_preparation),
            ExplainAnalyzeProjectionApplyResultV1::Inserted { .. }
        ));
        let node = &graph.nodes()[0];
        assert!(!node.start_observed);
        assert!(node.terminal_observed);
        assert_eq!(node.start_elapsed_ms, 12);
        assert_eq!(node.end_elapsed_ms, Some(33));
        assert_eq!(
            node.context
                .as_ref()
                .unwrap()
                .budget
                .as_ref()
                .unwrap()
                .visible_tool_count,
            3
        );
        assert_eq!(graph.nodes().len(), 1);
    }

    #[test]
    fn start_and_terminal_facts_merge_without_losing_observation_state() {
        let start = started(
            "provider-1",
            ExplainAnalyzeNodeKindV1::ProviderAttempt,
            Some("round-1"),
            "clock-1",
            10,
        );
        let terminal = finished(start.clone(), 10, 45);
        let mut graph = ExplainAnalyzeGraphV1::default();
        assert!(matches!(
            graph.apply(start),
            ExplainAnalyzeProjectionApplyResultV1::Inserted { node_index: 0 }
        ));
        assert!(matches!(
            graph.apply(terminal),
            ExplainAnalyzeProjectionApplyResultV1::Updated { node_index: 0 }
        ));
        let node = &graph.nodes()[0];
        assert!(node.start_observed);
        assert!(node.terminal_observed);
        assert_eq!(node.duration_ms, Some(35));
        assert_eq!(node.parent_node_id.as_deref(), Some("round-1"));
    }

    #[test]
    fn unresolved_parent_and_dependency_resolve_when_their_facts_arrive() {
        let mut child = started(
            "child",
            ExplainAnalyzeNodeKindV1::ToolCall,
            Some("parent"),
            "clock-1",
            5,
        );
        child.dependency_node_ids = vec!["dependency".to_owned()];
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(child);
        assert!(is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::MissingParent,
            Some("child"),
            Some("parent")
        ));
        assert!(is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::MissingDependency,
            Some("child"),
            Some("dependency")
        ));
        assert_eq!(graph.roots().collect::<Vec<_>>(), vec![0]);

        graph.apply(started(
            "parent",
            ExplainAnalyzeNodeKindV1::Turn,
            None,
            "clock-1",
            0,
        ));
        graph.apply(started(
            "dependency",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-1",
            1,
        ));
        assert!(!is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::MissingParent,
            Some("child"),
            Some("parent")
        ));
        assert!(!is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::MissingDependency,
            Some("child"),
            Some("dependency")
        ));
        assert_eq!(graph.nodes()[0].parent_index, Some(1));
        assert_eq!(graph.children(1), &[0]);
        assert_eq!(graph.nodes()[0].dependency_indices, vec![Some(2)]);
    }

    #[test]
    fn terminal_turn_reports_open_nodes_and_clears_them_when_finished() {
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(started(
            "round",
            ExplainAnalyzeNodeKindV1::ModelRound,
            Some("turn"),
            "clock-1",
            1,
        ));
        graph.apply(finished(
            started("turn", ExplainAnalyzeNodeKindV1::Turn, None, "clock-1", 0),
            0,
            30,
        ));
        assert!(is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode,
            Some("round"),
            None
        ));

        graph.apply(finished(
            started(
                "round",
                ExplainAnalyzeNodeKindV1::ModelRound,
                Some("turn"),
                "clock-1",
                1,
            ),
            1,
            20,
        ));
        assert!(!is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode,
            Some("round"),
            None
        ));
    }

    #[test]
    fn coverage_gaps_project_from_terminal_turn_without_changing_integrity() {
        let mut turn = finished(
            started("turn", ExplainAnalyzeNodeKindV1::Turn, None, "clock-1", 0),
            0,
            100,
        );
        turn.coverage_gaps = vec![
            crate::ExplainAnalyzeCoverageGapV1::ChildRunIntervals,
            crate::ExplainAnalyzeCoverageGapV1::ToolIoWaitIntervals,
        ];
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(turn);
        graph.finish_ingest();

        assert_eq!(
            graph.coverage_gaps(),
            vec![
                crate::ExplainAnalyzeCoverageGapV1::ChildRunIntervals,
                crate::ExplainAnalyzeCoverageGapV1::ToolIoWaitIntervals,
            ]
        );
        assert_eq!(
            graph.integrity(),
            ExplainAnalyzeGraphIntegrityV1::Consistent
        );
    }

    #[test]
    fn conflicting_terminal_turn_uses_the_merged_nodes_scope() {
        let mut open_a = started(
            "open-a",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-a",
            1,
        );
        open_a.turn_id = "turn-a".to_owned();
        let mut open_b = started(
            "open-b",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-b",
            2,
        );
        open_b.turn_id = "turn-b".to_owned();
        let mut turn_start = started(
            "turn-node",
            ExplainAnalyzeNodeKindV1::Turn,
            None,
            "clock-a",
            0,
        );
        turn_start.turn_id = "turn-a".to_owned();

        let mut conflicting_terminal = finished(
            started(
                "turn-node",
                ExplainAnalyzeNodeKindV1::Turn,
                None,
                "clock-b",
                0,
            ),
            0,
            30,
        );
        conflicting_terminal.turn_id = "turn-b".to_owned();

        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(open_a);
        graph.apply(open_b);
        graph.apply(turn_start);
        assert!(matches!(
            graph.apply(conflicting_terminal),
            ExplainAnalyzeProjectionApplyResultV1::Conflicted { .. }
        ));

        assert_eq!(graph.nodes()[2].turn_id, "turn-a");
        assert_eq!(graph.nodes()[2].clock_domain_id, "clock-a");
        assert!(is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode,
            Some("open-a"),
            None
        ));
        assert!(!is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode,
            Some("open-b"),
            None
        ));
    }

    #[test]
    fn conflicting_non_turn_terminal_still_closes_the_merged_turn_scope() {
        let mut open = started(
            "open-node",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-a",
            1,
        );
        open.turn_id = "turn-a".to_owned();
        let mut turn = started(
            "turn-node",
            ExplainAnalyzeNodeKindV1::Turn,
            None,
            "clock-a",
            0,
        );
        turn.turn_id = "turn-a".to_owned();
        let mut conflicting_terminal = finished(
            started(
                "turn-node",
                ExplainAnalyzeNodeKindV1::ToolCall,
                None,
                "clock-b",
                0,
            ),
            0,
            30,
        );
        conflicting_terminal.turn_id = "turn-b".to_owned();

        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(open);
        graph.apply(turn);
        assert!(matches!(
            graph.apply(conflicting_terminal),
            ExplainAnalyzeProjectionApplyResultV1::Conflicted { .. }
        ));

        assert_eq!(graph.nodes()[1].kind, ExplainAnalyzeNodeKindV1::Turn);
        assert!(graph.nodes()[1].terminal_observed);
        assert!(is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode,
            Some("open-node"),
            None
        ));
        assert!(!graph.diagnostics().iter().any(|diagnostic| diagnostic.code
            == ExplainAnalyzeProjectionDiagnosticCodeV1::UnresolvedTerminalNode
            && diagnostic.node_id.as_deref() == Some("turn-node")));
    }

    #[test]
    fn duplicate_event_identity_is_idempotent_and_conflict_marks_both_ids() {
        let first = started(
            "first",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-1",
            1,
        );
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(first.clone());
        assert_eq!(
            graph.apply(first.clone()),
            ExplainAnalyzeProjectionApplyResultV1::Duplicate {
                node_index: Some(0)
            }
        );
        assert_eq!(graph.duplicate_event_count(), 1);

        let mut reused_id = started(
            "second",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-1",
            2,
        );
        reused_id.event_id = first.event_id.clone();
        assert_eq!(
            graph.apply(reused_id),
            ExplainAnalyzeProjectionApplyResultV1::Conflicted {
                node_indices: vec![0],
                node_ids: vec!["first".to_owned(), "second".to_owned()],
            }
        );
        assert!(graph.nodes()[0].conflicted);
        assert_eq!(
            graph.conflicted_node_ids().collect::<Vec<_>>(),
            vec!["first", "second"]
        );

        let later_fact = started(
            "second",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-1",
            2,
        );
        assert!(matches!(
            graph.apply(later_fact),
            ExplainAnalyzeProjectionApplyResultV1::Conflicted { .. }
        ));
        assert!(graph.nodes()[1].conflicted);
    }

    #[test]
    fn terminal_fact_conflict_is_diagnostic_and_graph_integrity_is_unknown() {
        let start = started(
            "call",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-1",
            5,
        );
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(start.clone());
        graph.apply(finished(start.clone(), 5, 20));
        let conflict = finished(start, 5, 21);
        assert!(matches!(
            graph.apply(conflict),
            ExplainAnalyzeProjectionApplyResultV1::Conflicted { .. }
        ));
        assert_eq!(graph.integrity(), ExplainAnalyzeGraphIntegrityV1::Unknown);
        assert!(is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::ConflictingFact,
            Some("call"),
            None
        ));
    }

    #[test]
    fn context_assembly_payload_stays_attached_to_its_node_only() {
        let mut event = finished(
            started(
                "assembly",
                ExplainAnalyzeNodeKindV1::ContextAssembly,
                Some("prep"),
                "clock-1",
                3,
            ),
            3,
            9,
        );
        event.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: None,
            assembly: Some(Box::new(ExplainAnalyzeContextAssemblyV1 {
                edge_memory_selection: Vec::new(),
                basis: ExplainAnalyzeContextAssemblyBasisV1::RuntimeTextEstimate,
                sources: vec![ExplainAnalyzeContextSourceV1 {
                    kind: ExplainAnalyzeContextSourceKindV1::Memory,
                    section_count: 2,
                    estimated_tokens: 55,
                }],
            })),
        });
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(event);
        assert_eq!(
            graph.nodes()[0]
                .context
                .as_ref()
                .unwrap()
                .assembly
                .as_ref()
                .unwrap()
                .sources[0]
                .estimated_tokens,
            55
        );
        assert!(graph.nodes()[0].usage.is_none());
    }

    #[test]
    fn concurrency_uses_leaf_intervals_and_never_adds_across_clock_domains() {
        let mut graph = ExplainAnalyzeGraphV1::default();
        let c1_parent = finished(
            started("turn-a", ExplainAnalyzeNodeKindV1::Turn, None, "clock-a", 0),
            0,
            10,
        );
        let c2_parent = finished(
            started("turn-b", ExplainAnalyzeNodeKindV1::Turn, None, "clock-b", 0),
            0,
            10,
        );
        graph.apply(c1_parent);
        graph.apply(c2_parent);
        for id in ["a-1", "a-2"] {
            graph.apply(finished(
                started(
                    id,
                    ExplainAnalyzeNodeKindV1::ToolCall,
                    Some("turn-a"),
                    "clock-a",
                    0,
                ),
                0,
                10,
            ));
        }
        graph.apply(finished(
            started(
                "b-1",
                ExplainAnalyzeNodeKindV1::ToolCall,
                Some("turn-b"),
                "clock-b",
                0,
            ),
            0,
            10,
        ));
        graph.finish_ingest();
        assert_eq!(
            graph.integrity(),
            ExplainAnalyzeGraphIntegrityV1::Consistent
        );
        assert_eq!(graph.max_concurrency(), Some(2));
    }

    #[test]
    fn admission_and_wait_intervals_do_not_count_as_parallel_work() {
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(finished(
            started("turn", ExplainAnalyzeNodeKindV1::Turn, None, "clock-1", 0),
            0,
            180,
        ));
        graph.apply(finished(
            started(
                "batch",
                ExplainAnalyzeNodeKindV1::ToolBatch,
                Some("turn"),
                "clock-1",
                0,
            ),
            0,
            180,
        ));
        graph.apply(finished(
            started(
                "admission-a",
                ExplainAnalyzeNodeKindV1::Admission,
                Some("batch"),
                "clock-1",
                0,
            ),
            0,
            70,
        ));
        graph.apply(finished(
            started(
                "approval-a",
                ExplainAnalyzeNodeKindV1::Wait,
                Some("admission-a"),
                "clock-1",
                10,
            ),
            10,
            70,
        ));
        graph.apply(finished(
            started(
                "tool-a",
                ExplainAnalyzeNodeKindV1::ToolCall,
                Some("batch"),
                "clock-1",
                70,
            ),
            70,
            120,
        ));
        graph.apply(finished(
            started(
                "admission-b",
                ExplainAnalyzeNodeKindV1::Admission,
                Some("batch"),
                "clock-1",
                0,
            ),
            0,
            120,
        ));
        graph.apply(finished(
            started(
                "approval-b",
                ExplainAnalyzeNodeKindV1::Wait,
                Some("admission-b"),
                "clock-1",
                80,
            ),
            80,
            120,
        ));
        graph.apply(finished(
            started(
                "tool-b",
                ExplainAnalyzeNodeKindV1::ToolCall,
                Some("batch"),
                "clock-1",
                120,
            ),
            120,
            170,
        ));
        graph.finish_ingest();

        assert_eq!(
            graph.integrity(),
            ExplainAnalyzeGraphIntegrityV1::Consistent
        );
        assert_eq!(
            graph.max_concurrency(),
            Some(1),
            "overlapping dispatch and approval waits must not inflate executed-work concurrency"
        );
    }

    #[test]
    fn cycle_diagnostics_are_iterative_and_cycles_remain_renderable() {
        let mut a = started(
            "a",
            ExplainAnalyzeNodeKindV1::ToolCall,
            Some("b"),
            "clock-1",
            1,
        );
        a.dependency_node_ids = vec!["b".to_owned()];
        let mut b = started(
            "b",
            ExplainAnalyzeNodeKindV1::ToolCall,
            Some("a"),
            "clock-1",
            2,
        );
        b.dependency_node_ids = vec!["a".to_owned()];
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(a);
        graph.apply(b);
        graph.finish_ingest();
        assert_eq!(graph.integrity(), ExplainAnalyzeGraphIntegrityV1::Unknown);
        assert!(
            graph.diagnostics().iter().any(|diagnostic| diagnostic.code
                == ExplainAnalyzeProjectionDiagnosticCodeV1::ParentCycle)
        );
        assert!(graph.diagnostics().iter().any(|diagnostic| diagnostic.code
            == ExplainAnalyzeProjectionDiagnosticCodeV1::DependencyCycle));
        assert!(!graph.roots().collect::<Vec<_>>().is_empty());
    }

    fn reachable_from_roots(graph: &ExplainAnalyzeGraphV1) -> HashSet<usize> {
        let mut seen = HashSet::new();
        let mut stack: Vec<_> = graph.roots().collect();
        while let Some(index) = stack.pop() {
            if seen.insert(index) {
                stack.extend(graph.children(index).iter().copied());
            }
        }
        seen
    }

    #[test]
    fn roots_cover_independent_cycles_before_and_after_ingest_and_append() {
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(started(
            "ordinary-root",
            ExplainAnalyzeNodeKindV1::Turn,
            None,
            "clock-1",
            0,
        ));
        graph.apply(started(
            "cycle-a",
            ExplainAnalyzeNodeKindV1::ToolCall,
            Some("cycle-b"),
            "clock-1",
            1,
        ));
        graph.apply(started(
            "cycle-b",
            ExplainAnalyzeNodeKindV1::ToolCall,
            Some("cycle-a"),
            "clock-1",
            2,
        ));
        assert_eq!(reachable_from_roots(&graph).len(), 3);

        graph.finish_ingest();
        assert_eq!(reachable_from_roots(&graph).len(), 3);
        graph.apply(started(
            "cycle-child",
            ExplainAnalyzeNodeKindV1::ToolCall,
            Some("cycle-a"),
            "clock-1",
            3,
        ));
        assert_eq!(reachable_from_roots(&graph).len(), 4);

        graph.finish_ingest();
        assert_eq!(reachable_from_roots(&graph).len(), 4);
    }

    #[test]
    fn dependency_cycle_scan_handles_ten_thousand_edges_on_one_node() {
        const DEGREE: usize = 10_000;
        let mut root = started(
            "fanout-root",
            ExplainAnalyzeNodeKindV1::ToolBatch,
            None,
            "clock-1",
            0,
        );
        root.dependency_node_ids = (0..DEGREE)
            .map(|index| format!("dependency-{index}"))
            .collect();

        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(root);
        for index in 0..DEGREE {
            let mut dependency = started(
                &format!("dependency-{index}"),
                ExplainAnalyzeNodeKindV1::ToolCall,
                None,
                "clock-1",
                index as u64 + 1,
            );
            if index == 0 {
                dependency.dependency_node_ids = vec!["fanout-root".to_owned()];
            }
            graph.apply(dependency);
        }
        graph.finish_ingest();

        assert_eq!(graph.nodes()[0].dependency_indices.len(), DEGREE);
        assert!(
            graph.nodes()[0]
                .dependency_indices
                .iter()
                .all(Option::is_some)
        );
        assert!(is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::DependencyCycle,
            Some("fanout-root"),
            Some("dependency-0")
        ));
        assert_eq!(
            graph
                .diagnostics()
                .iter()
                .filter(|diagnostic| diagnostic.code
                    == ExplainAnalyzeProjectionDiagnosticCodeV1::DependencyCycle)
                .count(),
            1
        );
    }

    #[test]
    fn ten_thousand_events_update_the_arena_without_rebuilding_snapshots() {
        const NODE_COUNT: usize = 10_000;
        let mut graph = ExplainAnalyzeGraphV1::default();
        for index in 0..NODE_COUNT {
            let parent_id = (index > 0).then(|| format!("node-{}", index - 1));
            let mut event = started(
                &format!("node-{index}"),
                ExplainAnalyzeNodeKindV1::ToolCall,
                parent_id.as_deref(),
                "clock-1",
                index as u64,
            );
            event.event_id = format!("event-{index}");
            graph.apply(event);
        }
        assert_eq!(graph.nodes().len(), NODE_COUNT);
        assert_eq!(graph.children(NODE_COUNT - 2), &[NODE_COUNT - 1]);
        graph.finish_ingest();
        assert!(graph.diagnostics().is_empty());
    }

    #[test]
    fn invalid_events_are_counted_without_mutating_existing_nodes() {
        let event = started(
            "call",
            ExplainAnalyzeNodeKindV1::ToolCall,
            None,
            "clock-1",
            5,
        );
        let mut graph = ExplainAnalyzeGraphV1::default();
        graph.apply(event.clone());
        let mut invalid = event;
        invalid.label.clear();
        assert_eq!(
            graph.apply(invalid),
            ExplainAnalyzeProjectionApplyResultV1::Invalid
        );
        assert_eq!(graph.nodes().len(), 1);
        assert_eq!(graph.diagnostics().len(), 1);
        assert!(is_diagnostic(
            &graph,
            ExplainAnalyzeProjectionDiagnosticCodeV1::InvalidEvent,
            None,
            None
        ));
    }
}
