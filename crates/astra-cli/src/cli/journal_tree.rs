//! `astra journal tree <session>` — render a delegation / spawn tree.
//!
//! The runtime emits `DelegationStarted`, `DelegationSubRunStarted`, and
//! `DelegationSubRunCompleted` events (see
//! `astra_services::session_journal::JournalEventType`). For a multi-agent
//! session, stitching those events into a nested tree is the first thing a
//! reviewer does by hand via grep + mental math. This subcommand
//! automates it and prints either ASCII art (default) or a JSON blob for
//! downstream tooling.
//!
//! Scope cut in this first landing:
//! - Single-session view (no cross-session lineage). `SessionFork` events
//!   are recorded but not rendered as edges here.
//! - Delegation events only. Dynamic agent-spawn activity journals under the
//!   same plumbing as delegation today, so both show up; if they
//!   diverge we add a second event-type bucket here.
//!
//! # Output format
//!
//! Text:
//!
//! ```text
//! root [sess-abc] model=claude-sonnet-4-6 tokens=12k/450
//! ├── coder  tokens=?/? tool_calls=?
//! └── reviewer  tokens=?/? tool_calls=?
//! ```
//!
//! JSON mirrors `DelegationNode` verbatim so dashboards can deserialize
//! directly.

use serde::{Deserialize, Serialize};

use astra_services::session_journal::{JournalEvent, JournalEventType};

use crate::cli::cli_config::cli_args;
use crate::cli::journal_digest;
use crate::cli::journal_digest::display_known;
use crate::cli::surface::delegation_event_surface::{
    project_delegation_sub_run_completed, project_delegation_sub_run_started,
};

/// One node in the rendered tree. Leaf = no `children`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationNode {
    /// Stable identifier: the session id for root, sub-run id for a child.
    pub id: String,
    /// Human-readable label — "root" for the top-level session,
    /// "delegate#N" or the agent id for children.
    pub label: String,
    /// ISO 8601 start timestamp. Absent when the journal didn't
    /// record a start (e.g. a bare `DelegationCompleted` without a
    /// paired `DelegationStarted`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// ISO 8601 end timestamp. Absent for in-flight or
    /// never-terminated nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Model id reported for this sub-run, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Agent identity recorded by the sub-run producer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<String>,
    /// Raw model-round telemetry, separate from qualified terminal totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_round_input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_round_output: Option<u64>,
    /// Aggregated prompt tokens on the sub-run (includes children for root).
    pub prompt_tokens: u64,
    /// False means the value is only the known subtotal.
    pub prompt_tokens_complete: bool,
    /// Aggregated completion tokens on the sub-run (includes children for root).
    pub completion_tokens: u64,
    pub completion_tokens_complete: bool,
    /// Root's own known terminal prompt tokens (excludes children).
    pub self_prompt_tokens: u64,
    pub self_prompt_tokens_complete: bool,
    /// Root's own known terminal completion tokens (excludes children).
    pub self_completion_tokens: u64,
    pub self_completion_tokens_complete: bool,
    /// Known executed tool invocations, including children for the root.
    pub tool_calls: u32,
    pub tool_calls_complete: bool,
    /// Nested sub-runs with established parent relationships.
    pub children: Vec<DelegationNode>,
    /// Sub-runs whose parent cannot be established from retained events.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unattached: Vec<DelegationNode>,
}

/// Load a session journal, fold its delegation events into a tree.
/// Returns `(root, total_events, skipped_events)`. Skipped = events
/// whose metadata couldn't be parsed to fit the tree shape — counted,
/// not errored, so a rogue line doesn't break the whole command.
pub fn build_tree(session_id: &str) -> Result<(DelegationNode, u32, u32, Vec<String>), String> {
    let (events, incomplete, errors) = journal_digest::read_journal_for_analysis(session_id)?;
    let (root, total, skipped) = fold_events_with_coverage(session_id, &events, incomplete);
    Ok((root, total, skipped, errors))
}

pub(crate) fn fold_events_with_coverage(
    session_id: &str,
    events: &[JournalEvent],
    incomplete: bool,
) -> (DelegationNode, u32, u32) {
    let (mut root, total, skipped) = fold_events_into_tree(session_id, events);
    if incomplete {
        mark_coverage_incomplete(&mut root);
        root.tool_calls = super::journal_diff::confirmed_execution_floor(events.iter())
            .min(u64::from(u32::MAX)) as u32;
    }
    (root, total, skipped)
}

fn mark_coverage_incomplete(node: &mut DelegationNode) {
    node.prompt_tokens_complete = false;
    node.completion_tokens_complete = false;
    node.self_prompt_tokens_complete = false;
    node.self_completion_tokens_complete = false;
    node.tool_calls_complete = false;
    for child in node.children.iter_mut().chain(node.unattached.iter_mut()) {
        mark_coverage_incomplete(child);
    }
}

/// Pure fold: takes a session_id + event list, returns the tree.
/// Exposed as pub(crate) so tests can drive it without touching
/// the filesystem.
pub(crate) fn fold_events_into_tree(
    session_id: &str,
    events: &[JournalEvent],
) -> (DelegationNode, u32, u32) {
    use std::collections::HashMap;

    let observation_incomplete = journal_digest::has_incomplete_observation(events)
        || journal_digest::has_unmatched_root_rounds(events);
    let usage = journal_digest::root_terminal_usage(events, observation_incomplete);
    let mut root = DelegationNode {
        id: session_id.to_owned(),
        label: "root".to_owned(),
        started_at: None,
        ended_at: None,
        model: None,
        agent_id: None,
        parent_run_id: None,
        retry_of: None,
        observed_round_input: None,
        observed_round_output: None,
        prompt_tokens: usage.known.prompt,
        prompt_tokens_complete: usage.prompt_complete,
        completion_tokens: usage.known.completion,
        completion_tokens_complete: usage.completion_complete,
        self_prompt_tokens: usage.known.prompt,
        self_prompt_tokens_complete: usage.prompt_complete,
        self_completion_tokens: usage.known.completion,
        self_completion_tokens_complete: usage.completion_complete,
        tool_calls: 0,
        tool_calls_complete: events.iter().any(journal_digest::is_root_terminal)
            && events
                .iter()
                .filter(|event| journal_digest::is_root_terminal(event))
                .all(journal_digest::terminal_execution_complete)
            && !observation_incomplete
            && journal_digest::root_tool_observations_reconciled(events),
        children: Vec::new(),
        unattached: Vec::new(),
    };
    // Store lifecycle identity alongside each node; completion by run id alone
    // can incorrectly settle a different delegation or agent.
    let mut nodes: Vec<(DelegationNode, Option<String>, u64)> = Vec::new();
    let mut by_run_id = HashMap::new();
    let mut ambiguous_run_ids = std::collections::HashSet::new();
    let mut skipped: u32 = 0;
    for ev in events {
        if journal_digest::is_root_terminal(ev) {
            root.tool_calls = root.tool_calls.saturating_add(
                journal_digest::reported_executions(ev).min(u64::from(u32::MAX)) as u32,
            );
        }
        match ev.event_type {
            JournalEventType::Turn | JournalEventType::LlmRound
                if !journal_digest::is_child_producer(ev) =>
            {
                if root.started_at.is_none() && !ev.ts.is_empty() {
                    root.started_at = Some(ev.ts.clone());
                }
                root.ended_at = Some(ev.ts.clone());
                if root.model.is_none() {
                    root.model.clone_from(&ev.model);
                }
            }
            JournalEventType::DelegationSubRunStarted => {
                let projection = project_delegation_sub_run_started(ev.metadata.as_ref());
                let run_id = projection.sub_run_id;
                if run_id.trim().is_empty() {
                    skipped = skipped.saturating_add(1);
                    continue;
                }
                if by_run_id.contains_key(&run_id) {
                    ambiguous_run_ids.insert(run_id);
                    skipped = skipped.saturating_add(1);
                    continue;
                }
                let agent_id = (projection.agent_id != "?"
                    && !projection.agent_id.trim().is_empty())
                .then_some(projection.agent_id);
                let label = agent_id
                    .clone()
                    .unwrap_or_else(|| format!("delegate#{}", nodes.len() + 1));
                let metadata = ev.metadata.as_ref();
                let parent_run_id = metadata
                    .and_then(|m| m.get("parent_run_id"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned);
                let delegation_id = metadata
                    .and_then(|m| m.get("delegation_id"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned);
                let depth = metadata
                    .and_then(|m| m.get("depth"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let node = DelegationNode {
                    id: run_id.clone(),
                    label,
                    started_at: Some(ev.ts.clone()),
                    ended_at: None,
                    model: ev.model.clone(),
                    agent_id,
                    parent_run_id,
                    retry_of: projection.retry_of,
                    observed_round_input: None,
                    observed_round_output: None,
                    prompt_tokens: 0,
                    prompt_tokens_complete: false,
                    completion_tokens: 0,
                    completion_tokens_complete: false,
                    self_prompt_tokens: 0,
                    self_prompt_tokens_complete: false,
                    self_completion_tokens: 0,
                    self_completion_tokens_complete: false,
                    tool_calls: 0,
                    tool_calls_complete: false,
                    children: Vec::new(),
                    unattached: Vec::new(),
                };
                by_run_id.insert(run_id, nodes.len());
                nodes.push((node, delegation_id, depth));
            }
            _ => {}
        }
    }

    // Resolve completions after indexing all starts so a later conflicting
    // start cannot make an earlier completion belong to the wrong node.
    for ev in events
        .iter()
        .filter(|ev| ev.event_type == JournalEventType::DelegationSubRunCompleted)
    {
        let projection = project_delegation_sub_run_completed(ev.metadata.as_ref());
        let Some(&idx) = by_run_id.get(&projection.sub_run_id) else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        let (node, delegation, _) = &mut nodes[idx];
        let completed_delegation = ev
            .metadata
            .as_ref()
            .and_then(|m| m.get("delegation_id"))
            .and_then(serde_json::Value::as_str);
        if ambiguous_run_ids.contains(&projection.sub_run_id)
            || node.ended_at.is_some()
            || node.agent_id.as_deref() != Some(projection.agent_id.as_str())
            || delegation.as_deref().is_none()
            || delegation.as_deref() != completed_delegation
        {
            skipped = skipped.saturating_add(1);
            continue;
        }
        node.ended_at = Some(ev.ts.clone());
    }

    // Child LLM rounds are observations, not terminal accounting. A delegated
    // completion does not report usage, so no child total can be certified.
    let has_child_observation = journal_digest::has_child_observation(events);
    for event in events
        .iter()
        .filter(|event| journal_digest::is_child_producer(event))
    {
        if !matches!(
            event.event_type,
            JournalEventType::LlmRound
                | JournalEventType::ToolCallError
                | JournalEventType::Turn
                | JournalEventType::TurnError
        ) {
            continue;
        }
        let Some(run_id) = journal_digest::producer_run_id(event) else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        let Some(&idx) = by_run_id.get(run_id) else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        let node = &mut nodes[idx].0;
        let scope = event.producer_scope.as_ref();
        if ambiguous_run_ids.contains(run_id)
            || scope.is_some_and(|scope| {
                scope
                    .parent_run_id
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .zip(node.parent_run_id.as_deref())
                    .is_some_and(|(observed, started)| observed != started)
                    || scope
                        .agent_id
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .zip(node.agent_id.as_deref())
                        .is_some_and(|(observed, started)| observed != started)
            })
        {
            skipped = skipped.saturating_add(1);
            continue;
        }
        if node.model.is_none() {
            node.model.clone_from(&event.model);
        }
        if event.event_type == JournalEventType::LlmRound {
            if let Some(value) = event.tokens_in {
                let total = node.observed_round_input.get_or_insert(0);
                *total = total.saturating_add(value);
            }
            if let Some(value) = event.tokens_out {
                let total = node.observed_round_output.get_or_insert(0);
                *total = total.saturating_add(value);
            }
        }
    }

    // Depth belongs to the producer contract. Enforce increasing depth and a
    // bound when rendering a damaged journal so cycles cannot recurse forever.
    const MAX_RENDER_DEPTH: u64 = 64;
    let root_runs: std::collections::HashSet<&str> = events
        .iter()
        .filter(|event| {
            journal_digest::is_root_terminal(event)
                || (matches!(
                    event.event_type,
                    JournalEventType::LlmRound | JournalEventType::SessionStart
                ) && !journal_digest::is_child_producer(event))
        })
        .filter_map(|event| {
            let scoped = event
                .producer_scope
                .as_ref()
                .map(|scope| scope.run_id.as_str());
            let recorded = event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("run_id"))
                .and_then(serde_json::Value::as_str);
            if scoped
                .zip(recorded)
                .is_some_and(|(left, right)| left != right)
            {
                return None;
            }
            journal_digest::producer_run_id(event)
        })
        .collect();
    let mut children = vec![Vec::new(); nodes.len()];
    let mut root_indices = Vec::new();
    let mut unattached = Vec::new();
    for (idx, (node, _, depth)) in nodes.iter().enumerate() {
        if !ambiguous_run_ids.contains(&node.id)
            && *depth == 1
            && node
                .parent_run_id
                .as_deref()
                .is_some_and(|id| root_runs.contains(id) && !by_run_id.contains_key(id))
        {
            root_indices.push(idx);
        } else if !ambiguous_run_ids.contains(&node.id)
            && *depth > 1
            && *depth <= MAX_RENDER_DEPTH
            && let Some(&parent) = node
                .parent_run_id
                .as_deref()
                .filter(|id| !ambiguous_run_ids.contains(*id))
                .and_then(|id| by_run_id.get(id))
            && nodes[parent].2.checked_add(1) == Some(*depth)
        {
            children[parent].push(idx);
        } else {
            skipped = skipped.saturating_add(1);
            unattached.push(idx);
        }
    }
    fn take_tree(
        idx: usize,
        nodes: &mut [Option<DelegationNode>],
        children: &[Vec<usize>],
    ) -> DelegationNode {
        let mut node = nodes[idx].take().expect("each sub-run has one parent");
        node.children = children[idx]
            .iter()
            .map(|&child| take_tree(child, nodes, children))
            .collect();
        node
    }
    let mut nodes: Vec<_> = nodes.into_iter().map(|(node, _, _)| Some(node)).collect();
    root.children = root_indices
        .into_iter()
        .map(|idx| take_tree(idx, &mut nodes, &children))
        .collect();
    root.unattached = unattached
        .into_iter()
        .map(|idx| take_tree(idx, &mut nodes, &children))
        .collect();
    if !root.children.is_empty()
        || !root.unattached.is_empty()
        || has_child_observation
        || skipped > 0
    {
        root.prompt_tokens_complete = false;
        root.completion_tokens_complete = false;
        root.tool_calls_complete = false;
    }
    if !root.tool_calls_complete {
        root.tool_calls = super::journal_diff::confirmed_execution_floor(events.iter())
            .min(u64::from(u32::MAX)) as u32;
    }
    (root, events.len() as u32, skipped)
}

/// Render the tree as ASCII art into a String.
pub fn render_text(root: &DelegationNode) -> String {
    let mut out = String::new();
    render_node_text(root, "", true, &mut out, true);
    if !root.unattached.is_empty() {
        out.push_str("unattached sub-runs (parent not established):\n");
        for (i, node) in root.unattached.iter().enumerate() {
            render_node_text(node, "", i + 1 == root.unattached.len(), &mut out, false);
        }
    }
    out
}

fn render_node_text(
    node: &DelegationNode,
    prefix: &str,
    is_last: bool,
    out: &mut String,
    is_root: bool,
) {
    if is_root {
        out.push_str(&format!(
            "{} [{}] tokens={}/{} tool_calls={}\n",
            node.label,
            node.id,
            display_known(node.prompt_tokens, node.prompt_tokens_complete),
            display_known(node.completion_tokens, node.completion_tokens_complete),
            display_known(node.tool_calls, node.tool_calls_complete)
        ));
    } else {
        let connector = if is_last { "└── " } else { "├── " };
        out.push_str(&format!(
            "{prefix}{connector}{label}  tokens={}/{} tool_calls={}\n",
            display_known(node.prompt_tokens, node.prompt_tokens_complete),
            display_known(node.completion_tokens, node.completion_tokens_complete),
            display_known(node.tool_calls, node.tool_calls_complete),
            label = node.label
        ));
        let extra_prefix = if is_last { "    " } else { "│   " };
        let child_prefix = format!("{prefix}{extra_prefix}");
        if let Some(a) = node.agent_id.as_deref() {
            out.push_str(&format!("{child_prefix}agent_id={a}\n"));
        }
        if let Some(parent) = node.parent_run_id.as_deref() {
            out.push_str(&format!("{child_prefix}parent_run_id={parent}\n"));
        }
        if node.observed_round_input.is_some() || node.observed_round_output.is_some() {
            let input = node
                .observed_round_input
                .map_or_else(|| "?".to_owned(), |value| value.to_string());
            let output = node
                .observed_round_output
                .map_or_else(|| "?".to_owned(), |value| value.to_string());
            out.push_str(&format!(
                "{child_prefix}observed_round_tokens={input}/{output} (unqualified)\n"
            ));
        }
        if let Some(s) = node.started_at.as_deref()
            && let Some(e) = node.ended_at.as_deref()
        {
            out.push_str(&format!("{child_prefix}started={s} ended={e}\n"));
        }
    }
    let count = node.children.len();
    for (i, c) in node.children.iter().enumerate() {
        let child_prefix = if is_root {
            String::new()
        } else {
            let extra = if is_last { "    " } else { "│   " };
            format!("{prefix}{extra}")
        };
        render_node_text(c, &child_prefix, i + 1 == count, out, false);
    }
}

/// CLI entrypoint for `astra journal tree`.
pub(crate) fn run_tree(args: &cli_args::JournalTreeArgs) -> Result<(), String> {
    let session_id = journal_digest::resolve_session_for_digest(
        args.session_id.as_deref(),
        args.session.as_deref(),
    )?;
    let (root, total, skipped, source_errors) = build_tree(&session_id)?;
    let format = args.format.trim().to_ascii_lowercase();
    match format.as_str() {
        "" | "text" | "txt" => {
            let rendered = render_text(&root);
            stdout_print!("{rendered}");
            for error in &source_errors {
                eprintln!("[journal tree] source unavailable: {error}");
            }
            if skipped > 0 {
                eprintln!(
                    "[journal tree] note: skipped {skipped}/{total} events (unattachable SubRunCompleted, etc.)"
                );
            }
            Ok(())
        }
        "json" => {
            let body = serde_json::json!({
                "schema_version": "astra-journal-tree-v2",
                "session_id": session_id,
                "total_events": total,
                "skipped_events": skipped,
                "source_errors": source_errors,
                "root": root,
            });
            stdout_println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
            Ok(())
        }
        other => Err(format!(
            "invalid --format '{other}' (expected text or json)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{fold_events_into_tree, render_text};
    use astra_services::session_journal::JournalEvent;
    use serde_json::json;

    fn turn_event(ts: &str, prompt: u64, completion: u64, model: &str) -> JournalEvent {
        let raw = json!({
            "type": "turn",
            "ts": ts,
            "session_id": "sess-1",
            "producer_scope": {"run_id":"root-run"},
            "turn": 1,
            "model": model,
            "tokens_in": prompt,
            "tokens_out": completion,
        });
        serde_json::from_value(raw).expect("valid turn event")
    }

    fn sub_started(ts: &str, run_id: &str, agent_id: &str) -> JournalEvent {
        let mut event = JournalEvent::delegation_sub_run_started(
            Some("sess-1"),
            "delegation",
            run_id,
            "root-run",
            agent_id,
            "running",
            1,
            None,
        );
        event.ts = ts.to_owned();
        event
    }

    fn sub_completed(ts: &str, run_id: &str) -> JournalEvent {
        let mut event = JournalEvent::delegation_sub_run_completed(
            Some("sess-1"),
            "delegation",
            run_id,
            match run_id {
                "run-A" | "run-DUP" => "coder",
                "run-B" => "reviewer",
                _ => "worker",
            },
            "completed",
            None,
            None,
        );
        event.ts = ts.to_owned();
        event
    }

    fn child_round(
        ts: &str,
        run_id: &str,
        agent_id: &str,
        prompt: u64,
        completion: u64,
    ) -> JournalEvent {
        use astra_services::session_journal::{LlmRoundRecord, TurnEventBuffer};
        let mut buffer = TurnEventBuffer::begin_producer_turn(Some("sess-1"), 7);
        buffer.record_llm_round(LlmRoundRecord {
            prompt_tokens: prompt,
            completion_tokens: completion,
            run_id: Some(run_id.into()),
            parent_run_id: Some("root-run".into()),
            agent_id: Some(agent_id.into()),
            ..LlmRoundRecord::new(astra_turn_types::InferencePurpose::SubAgent)
        });
        let mut event = buffer.drain().pop().unwrap();
        event.ts = ts.to_owned();
        event
    }

    #[test]
    fn real_delegation_group_and_round_do_not_invent_child_or_qualified_usage() {
        let mut root_turn = turn_event("t0", 100, 10, "sonnet");
        root_turn.tool_count = Some(0);
        let mut group = JournalEvent::delegation_started(
            Some("sess-1"),
            "delegation",
            "root-run",
            "parallel",
            &["worker".into()],
        );
        group.ts = "t1".into();
        let mut started = JournalEvent::delegation_sub_run_started(
            Some("sess-1"),
            "delegation",
            "child-run",
            "root-run",
            "worker",
            "running",
            1,
            None,
        );
        started.ts = "t2".into();
        let round = child_round("t3", "child-run", "worker", 20, 2);
        assert_eq!(round.turn, None);
        assert_eq!(round.producer_scope.as_ref().unwrap().local_turn, Some(7));
        let completed = sub_completed("t5", "child-run");
        let events = vec![root_turn.clone(), group, started, round, completed];
        let (root, _, skipped) = fold_events_into_tree("sess-1", &events);
        assert_eq!(skipped, 0);
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].observed_round_input, Some(20));
        assert_eq!(root.children[0].observed_round_output, Some(2));
        assert_eq!(root.children[0].prompt_tokens, 0);
        assert!(!root.children[0].prompt_tokens_complete);
        assert_eq!(root.prompt_tokens, 100);
        assert!(!root.prompt_tokens_complete);
        assert!(render_text(&root).contains("observed_round_tokens=20/2 (unqualified)"));
    }

    #[test]
    fn tree_folds_turn_events_into_root_totals() {
        let events = vec![
            turn_event("2026-04-30T10:00:00Z", 1000, 100, "sonnet"),
            turn_event("2026-04-30T10:00:05Z", 500, 50, "sonnet"),
        ];
        let (root, total, skipped) = fold_events_into_tree("sess-1", &events);
        assert_eq!(total, 2);
        assert_eq!(skipped, 0);
        assert_eq!(root.prompt_tokens, 1500);
        assert_eq!(root.completion_tokens, 150);
        assert_eq!(root.model.as_deref(), Some("sonnet"));
        assert_eq!(root.started_at.as_deref(), Some("2026-04-30T10:00:00Z"));
        assert_eq!(root.ended_at.as_deref(), Some("2026-04-30T10:00:05Z"));
        assert!(root.children.is_empty());
    }

    #[test]
    fn tree_uses_a_lower_bound_for_partially_observed_execution() {
        let mut turn = turn_event("t1", 100, 10, "sonnet");
        turn.tool_count = Some(541);
        let mut round = turn.clone();
        round.event_type = astra_services::session_journal::JournalEventType::LlmRound;
        let mut failed = turn_event("t2", 20, 2, "sonnet");
        failed.event_type = astra_services::session_journal::JournalEventType::TurnError;
        failed.turn = Some(2);
        failed.tool_count = Some(9);
        failed.producer_scope = Some(
            serde_json::from_value(json!({"run_id":"other-root", "agent_id":"root"})).unwrap(),
        );
        let child = child_round("t3", "run-A", "coder", 20, 2);
        let events = vec![
            round,
            turn,
            failed,
            sub_started("t3", "run-A", "coder"),
            child,
            sub_completed("t4", "run-A"),
        ];
        let (root, _, skipped) = fold_events_into_tree("sess-1", &events);
        assert_eq!(skipped, 0);
        // Without invocation identities, the two terminal reports might overlap.
        assert_eq!(root.tool_calls, 541);
        assert_eq!(root.children[0].tool_calls, 0);
        assert!(!root.children[0].tool_calls_complete);
        assert!(render_text(&root).contains("tool_calls=partial(541)\n"));
    }

    #[test]
    fn unaccounted_child_does_not_make_root_inclusive_totals_complete() {
        let mut turn = turn_event("t1", 100, 10, "sonnet");
        turn.tool_count = Some(1);
        let mut child = turn.clone();
        child.event_type = astra_services::session_journal::JournalEventType::LlmRound;
        child.metadata = Some(json!({"purpose": "sub_agent"}));
        let (root, _, _) = fold_events_into_tree("sess-1", &[turn.clone(), child.clone()]);
        assert_eq!(root.self_prompt_tokens, 100);
        assert!(root.self_prompt_tokens_complete);
        assert!(root.self_completion_tokens_complete);
        assert!(!root.prompt_tokens_complete);
        assert!(!root.completion_tokens_complete);
        assert!(!root.tool_calls_complete);

        child.metadata = Some(json!({"purpose": "sub_agent", "run_id": "run-A"}));
        child.producer_scope.as_mut().unwrap().run_id = "run-A".into();
        let (root, _, _) = fold_events_into_tree(
            "sess-1",
            &[
                turn,
                sub_started("t2", "run-A", "coder"),
                child,
                sub_completed("t3", "run-A"),
            ],
        );
        assert!(!root.prompt_tokens_complete);
        assert!(!root.completion_tokens_complete);
        assert!(!root.tool_calls_complete);
        assert_eq!(root.prompt_tokens, 100);
        assert_eq!(root.children[0].observed_round_input, Some(100));
    }

    #[test]
    fn tree_does_not_certify_a_zero_that_contradicts_executed_detail() {
        let mut turn = turn_event("t1", 0, 0, "sonnet");
        turn.tool_count = Some(0);
        turn.tool_calls =
            Some(serde_json::from_value(json!([{"name":"read_file", "ok":true, "ms":1}])).unwrap());
        let (root, _, _) = fold_events_into_tree("sess-1", &[turn]);
        assert_eq!(root.tool_calls, 1);
        assert!(!root.tool_calls_complete);

        let mut rejected = turn_event("t1", 0, 0, "sonnet");
        rejected.tool_count = Some(0);
        rejected.tool_calls = Some(
            serde_json::from_value(json!([{"name":"read_file", "ok":false, "ms":0,
                "disposition":"rejected"}]))
            .unwrap(),
        );
        let (root, _, _) = fold_events_into_tree("sess-1", &[rejected]);
        assert_eq!(root.tool_calls, 0);
        assert!(root.tool_calls_complete);
    }

    #[test]
    fn incomplete_tree_keeps_identified_child_execution_evidence() {
        let mut turn = turn_event("t1", 0, 0, "root");
        turn.tool_count = Some(0);
        let mut child = child_round("t3", "run-A", "coder", 10, 1);
        child.tool_calls = Some(
            serde_json::from_value(json!([{"name":"bash","ok":true,"ms":1,
                "tool_call_id":"call","invocation_identity":{"user_id":"owner",
                "session_id":"sess-1","run_id":"run-A","turn_chain_id":"chain",
                "invocation_id":"call"}}]))
            .unwrap(),
        );
        let events = [turn, sub_started("t2", "run-A", "coder"), child];
        let (root, _, _) = fold_events_into_tree("sess-1", &events);
        assert_eq!(root.tool_calls, 1);
        assert!(!root.tool_calls_complete);
        let diff = crate::cli::journal_diff::diff_events("sess-1", "other", &events, &[]);
        assert_eq!(diff.a_tool_calls_executed, 1);
        assert!(!diff.a_tool_sequence_complete);
    }

    #[test]
    fn tree_saturates_root_and_delegated_tool_counts() {
        let mut turn = turn_event("t1", 0, 0, "sonnet");
        turn.tool_count = Some(u32::MAX);
        let mut second_turn = turn.clone();
        second_turn.turn = Some(2);
        let events = vec![
            turn,
            second_turn,
            sub_started("t2", "run-A", "coder"),
            sub_completed("t3", "run-A"),
        ];
        let (root, _, _) = fold_events_into_tree("sess-1", &events);
        assert_eq!(root.tool_calls, u32::MAX);
    }

    #[test]
    fn tree_attaches_sub_runs_by_run_id() {
        let events = vec![
            turn_event("2026-04-30T10:00:00Z", 1000, 100, "sonnet"),
            sub_started("2026-04-30T10:00:01Z", "run-A", "coder"),
            sub_started("2026-04-30T10:00:02Z", "run-B", "reviewer"),
            child_round("2026-04-30T10:00:09Z", "run-A", "coder", 2000, 300),
            child_round("2026-04-30T10:00:11Z", "run-B", "reviewer", 500, 100),
            sub_completed("2026-04-30T10:00:10Z", "run-A"),
            sub_completed("2026-04-30T10:00:12Z", "run-B"),
        ];
        let (root, _total, skipped) = fold_events_into_tree("sess-1", &events);
        assert_eq!(skipped, 0);
        assert_eq!(root.children.len(), 2);
        let a = root.children.iter().find(|n| n.id == "run-A").unwrap();
        assert_eq!(a.label, "coder");
        assert_eq!(a.observed_round_input, Some(2000));
        assert_eq!(a.observed_round_output, Some(300));
        assert!(!a.tool_calls_complete);
        assert_eq!(a.agent_id.as_deref(), Some("coder"));
        assert_eq!(a.ended_at.as_deref(), Some("2026-04-30T10:00:10Z"));
        let b = root.children.iter().find(|n| n.id == "run-B").unwrap();
        assert_eq!(b.label, "reviewer");
        // Child rounds are observations, not qualified terminal usage.
        assert_eq!(root.prompt_tokens, 1000);
        assert_eq!(root.completion_tokens, 100);
        assert!(!root.prompt_tokens_complete);
    }

    #[test]
    fn nested_and_unattached_subruns_preserve_parent_and_completion_identity() {
        let mut nested = JournalEvent::delegation_sub_run_started(
            Some("sess-1"),
            "nested",
            "run-nested",
            "run-A",
            "worker",
            "running",
            2,
            None,
        );
        nested.ts = "t3".into();
        let wrong_completion = JournalEvent::delegation_sub_run_completed(
            Some("sess-1"),
            "wrong",
            "run-nested",
            "worker",
            "completed",
            None,
            None,
        );
        let orphan = JournalEvent::delegation_sub_run_started(
            Some("sess-1"),
            "other",
            "orphan",
            "missing-parent",
            "worker",
            "running",
            2,
            None,
        );
        let events = vec![
            turn_event("t0", 0, 0, "root"),
            sub_started("t1", "run-A", "coder"),
            nested,
            wrong_completion,
            orphan,
        ];
        let (root, _, skipped) = fold_events_into_tree("sess-1", &events);
        assert_eq!(skipped, 2);
        assert_eq!(root.children[0].children[0].id, "run-nested");
        assert_eq!(root.children[0].children[0].ended_at, None);
        assert_eq!(root.unattached[0].id, "orphan");
        assert!(render_text(&root).contains("unattached sub-runs"));
    }

    #[test]
    fn only_known_root_runs_can_parent_top_level_children() {
        let mut other_root = turn_event("t0", 0, 0, "root");
        other_root.producer_scope.as_mut().unwrap().run_id = "other-root".into();
        let mut unattached = sub_started("t3", "orphan", "worker");
        unattached.metadata.as_mut().unwrap()["parent_run_id"] = json!("missing-root");
        let mut other_child = sub_started("t2", "other-child", "worker");
        other_child.metadata.as_mut().unwrap()["parent_run_id"] = json!("other-root");
        // Events can arrive out of order; both explicit root identities remain usable.
        let events = vec![
            unattached,
            other_child,
            sub_started("t1", "run-A", "coder"),
            other_root,
            turn_event("t0", 0, 0, "root"),
        ];
        let (root, _, _) = fold_events_into_tree("sess-1", &events);
        assert_eq!(
            root.children
                .iter()
                .map(|child| child.id.as_str())
                .collect::<Vec<_>>(),
            ["other-child", "run-A"]
        );
        assert_eq!(root.unattached[0].id, "orphan");

        // Conflicting root identities cannot establish either parent.
        let mut conflict = turn_event("t0", 0, 0, "root");
        conflict.metadata = Some(json!({"run_id":"other-root"}));
        let (root, _, _) =
            fold_events_into_tree("sess-1", &[conflict, sub_started("t1", "run-A", "coder")]);
        assert!(root.children.is_empty());
        assert_eq!(root.unattached[0].id, "run-A");

        let (root, _, _) = fold_events_into_tree("sess-1", &[sub_started("t1", "run-A", "coder")]);
        assert!(root.children.is_empty());
        assert_eq!(root.unattached[0].id, "run-A");
    }

    #[test]
    fn completion_without_start_keeps_inclusive_coverage_unknown() {
        let mut turn = turn_event("t1", 100, 10, "root");
        turn.tool_count = Some(0);
        let events = vec![turn, sub_completed("t2", "child-run")];
        let (tree, _, _) = fold_events_into_tree("sess-1", &events);
        assert!(!tree.prompt_tokens_complete);
        assert!(!tree.tool_calls_complete);
        let diff = crate::cli::journal_diff::diff_events("sess-1", "other", &events, &[]);
        assert!(!diff.a_tool_sequence_complete);
    }

    #[test]
    fn child_round_cannot_set_root_model_or_timestamps() {
        let mut child = child_round("t0", "run-A", "coder", 20, 2);
        child.model = Some("child-model".into());
        let root_turn = turn_event("t1", 100, 10, "root-model");
        let (root, _, _) = fold_events_into_tree("sess-1", &[child, root_turn]);
        assert_eq!(root.started_at.as_deref(), Some("t1"));
        assert_eq!(root.model.as_deref(), Some("root-model"));
    }

    #[test]
    fn tree_counts_skipped_for_unattachable_completions() {
        let events = vec![
            // Completed without a started — can't attach.
            sub_completed("2026-04-30T10:00:10Z", "run-ghost"),
            // Completed with no run_id at all in metadata.
            {
                let raw = json!({
                    "type": "delegation_sub_run_completed",
                    "ts": "2026-04-30T10:00:11Z",
                    "session_id": "sess-1",
                    "metadata": {}
                });
                serde_json::from_value::<JournalEvent>(raw).unwrap()
            },
        ];
        let (root, total, skipped) = fold_events_into_tree("sess-1", &events);
        assert_eq!(total, 2);
        assert_eq!(skipped, 2);
        assert!(root.children.is_empty());
    }

    #[test]
    fn render_text_includes_ascii_connectors_and_child_labels() {
        let events = vec![
            turn_event("2026-04-30T10:00:00Z", 100, 10, "sonnet"),
            sub_started("2026-04-30T10:00:01Z", "run-A", "coder"),
            sub_completed("2026-04-30T10:00:10Z", "run-A"),
        ];
        let (root, _, _) = fold_events_into_tree("sess-1", &events);
        let txt = render_text(&root);
        assert!(txt.contains("root [sess-1]"));
        assert!(txt.contains("└── coder"));
        assert!(txt.contains("agent_id=coder"));
        assert!(txt.contains("started=2026-04-30T10:00:01Z ended=2026-04-30T10:00:10Z"));
    }

    #[test]
    fn tree_root_exposes_self_tokens_separate_from_total() {
        // Child round telemetry cannot be added to terminal accounting.
        let events = vec![
            turn_event("t1", 1000, 100, "sonnet"),
            sub_started("t2", "run-A", "coder"),
            child_round("t3", "run-A", "coder", 2000, 300),
            sub_completed("t3", "run-A"),
        ];
        let (root, _, _) = fold_events_into_tree("sess-1", &events);
        assert_eq!(root.prompt_tokens, 1000);
        assert_eq!(root.completion_tokens, 100);
        assert!(!root.prompt_tokens_complete);
        // Self = root own only
        assert_eq!(
            root.self_prompt_tokens, 1000,
            "self_prompt must be root's own Turn tokens only"
        );
        assert_eq!(
            root.self_completion_tokens, 100,
            "self_completion must be root's own Turn tokens only"
        );
    }

    #[test]
    fn duplicate_run_ids_do_not_establish_a_parent() {
        let mut nested = JournalEvent::delegation_sub_run_started(
            Some("sess-1"),
            "nested",
            "nested-run",
            "run-DUP",
            "worker",
            "running",
            2,
            None,
        );
        nested.ts = "t4".into();
        let events = vec![
            turn_event("t0", 0, 0, "root"),
            sub_started("t1", "run-DUP", "coder"),
            sub_started("t2", "run-DUP", "reviewer"),
            sub_completed("t3", "run-DUP"),
            nested,
        ];
        let (root, _, skipped) = fold_events_into_tree("sess-1", &events);
        assert!(skipped >= 1);
        assert!(root.children.is_empty());
        assert_eq!(
            root.unattached
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["run-DUP", "nested-run"]
        );
        assert!(root.unattached[0].children.is_empty());
    }

    #[test]
    fn render_text_emits_two_children_with_branch_and_last_connectors() {
        let events = vec![
            sub_started("t1", "run-A", "coder"),
            sub_started("t2", "run-B", "reviewer"),
            sub_completed("t3", "run-A"),
            sub_completed("t4", "run-B"),
        ];
        let (root, _, _) = fold_events_into_tree("sess-1", &events);
        let txt = render_text(&root);
        assert!(
            txt.contains("├── coder"),
            "non-last child should use branch connector: {txt}"
        );
        assert!(
            txt.contains("└── reviewer"),
            "last child should use corner connector: {txt}"
        );
    }
}
