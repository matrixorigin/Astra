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
//! - Delegation events only. `spawn_agent` (dynamic) journals under the
//!   same plumbing as delegation today, so both show up; if they
//!   diverge we add a second event-type bucket here.
//!
//! # Output format
//!
//! Text:
//!
//! ```text
//! root [sess-abc] model=claude-sonnet-4-6 tokens=12k/450
//! ├── delegate#1 started=T+2.1s ended=T+5.3s tokens=8k/200
//! │   agent_type=coder task="refactor registry.rs"
//! └── delegate#2 started=T+2.8s ended=T+4.9s tokens=2.5k/150
//!     agent_type=reviewer task="review the refactor"
//! ```
//!
//! JSON mirrors `DelegationNode` verbatim so dashboards can deserialize
//! directly.

use serde::{Deserialize, Serialize};

use astra_services::session_journal::{self, JournalEvent, JournalEventType};

use crate::cli_args;
use crate::journal_digest;

/// One node in the rendered tree. Leaf = no `children`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationNode {
    /// Stable identifier for this node. Root node uses the session id;
    /// sub-run nodes use the sub-run's run_id if available, else a
    /// synthetic "delegate-<seq>".
    pub id: String,
    /// Human-readable label — "root" for the top-level session,
    /// "delegate#N" or the agent_type name for children.
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
    /// Agent type hint from the delegation config, e.g. "coder".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Task summary (truncated). Free-form from the delegation call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Aggregated prompt tokens on the sub-run (includes children for root).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub prompt_tokens: u64,
    /// Aggregated completion tokens on the sub-run (includes children for root).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub completion_tokens: u64,
    /// Root's own prompt tokens (Turn/LlmRound only, excludes children).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub self_prompt_tokens: u64,
    /// Root's own completion tokens (Turn/LlmRound only, excludes children).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub self_completion_tokens: u64,
    /// Tool invocations on this sub-run.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub tool_calls: u32,
    /// Nested sub-runs.
    pub children: Vec<DelegationNode>,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// Load a session journal, fold its delegation events into a tree.
/// Returns `(root, total_events, skipped_events)`. Skipped = events
/// whose metadata couldn't be parsed to fit the tree shape — counted,
/// not errored, so a rogue line doesn't break the whole command.
pub fn build_tree(session_id: &str) -> Result<(DelegationNode, u32, u32), String> {
    let events = session_journal::read_journal(session_id).map_err(|e| e.to_string())?;
    Ok(fold_events_into_tree(session_id, &events))
}

/// Pure fold: takes a session_id + event list, returns the tree.
/// Exposed as pub(crate) so tests can drive it without touching
/// the filesystem.
pub(crate) fn fold_events_into_tree(
    session_id: &str,
    events: &[JournalEvent],
) -> (DelegationNode, u32, u32) {
    let mut root = DelegationNode {
        id: session_id.to_string(),
        label: "root".to_string(),
        started_at: None,
        ended_at: None,
        model: None,
        agent_type: None,
        task: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        self_prompt_tokens: 0,
        self_completion_tokens: 0,
        tool_calls: 0,
        children: Vec::new(),
    };
    let mut seq: u32 = 0;
    let mut skipped: u32 = 0;
    let mut by_run_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for ev in events {
        match ev.event_type {
            JournalEventType::Turn | JournalEventType::LlmRound => {
                // Root-level token accumulation. Use prompt_tokens +
                // completion_tokens when present.
                if let Some(p) = ev.tokens_in {
                    root.prompt_tokens = root.prompt_tokens.saturating_add(p);
                }
                if let Some(c) = ev.tokens_out {
                    root.completion_tokens = root.completion_tokens.saturating_add(c);
                }
                if root.started_at.is_none() && !ev.ts.is_empty() {
                    root.started_at = Some(ev.ts.clone());
                }
                // Last seen ts becomes the end — approximate, but
                // good enough for a tree overview.
                root.ended_at = Some(ev.ts.clone());
                if root.model.is_none()
                    && let Some(m) = ev.model.as_deref()
                {
                    root.model = Some(m.to_string());
                }
            }
            JournalEventType::DelegationSubRunStarted
            | JournalEventType::DelegationStarted => {
                seq = seq.saturating_add(1);
                let run_id = ev
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("run_id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("delegate-{seq}"));
                let agent_type = ev
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("agent_type"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let task = ev
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("task"))
                    .and_then(|v| v.as_str())
                    .map(|s| truncate(s, 120));
                let label = agent_type
                    .clone()
                    .unwrap_or_else(|| format!("delegate#{seq}"));
                let node = DelegationNode {
                    id: run_id.clone(),
                    label,
                    started_at: Some(ev.ts.clone()),
                    ended_at: None,
                    model: ev.model.clone(),
                    agent_type,
                    task,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    self_prompt_tokens: 0,
                    self_completion_tokens: 0,
                    tool_calls: 0,
                    children: Vec::new(),
                };
                root.children.push(node);
                by_run_id.insert(run_id, root.children.len() - 1);
            }
            JournalEventType::DelegationSubRunCompleted => {
                let run_id = ev
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("run_id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let Some(rid) = run_id else {
                    // Completed event without a run_id — we can't
                    // attach to any specific child. Count and move on.
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                let Some(&idx) = by_run_id.get(&rid) else {
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                let child = &mut root.children[idx];
                child.ended_at = Some(ev.ts.clone());
                if let Some(p) = ev.tokens_in {
                    child.prompt_tokens = child.prompt_tokens.saturating_add(p);
                }
                if let Some(c) = ev.tokens_out {
                    child.completion_tokens = child.completion_tokens.saturating_add(c);
                }
                if let Some(t) = ev.tool_count {
                    child.tool_calls = child.tool_calls.saturating_add(t);
                }
            }
            _ => { /* ignore: irrelevant for the tree view */ }
        }
    }

    // Snapshot root's own tokens before roll-up.
    root.self_prompt_tokens = root.prompt_tokens;
    root.self_completion_tokens = root.completion_tokens;

    // Roll-up aggregation into the root for a quick summary.
    for child in &root.children {
        root.prompt_tokens = root.prompt_tokens.saturating_add(child.prompt_tokens);
        root.completion_tokens =
            root.completion_tokens.saturating_add(child.completion_tokens);
        root.tool_calls = root.tool_calls.saturating_add(child.tool_calls);
    }

    (root, events.len() as u32, skipped)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Render the tree as ASCII art into a String.
pub fn render_text(root: &DelegationNode) -> String {
    let mut out = String::new();
    render_node_text(root, "", true, &mut out, true);
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
            node.prompt_tokens,
            node.completion_tokens,
            node.tool_calls
        ));
    } else {
        let connector = if is_last { "└── " } else { "├── " };
        out.push_str(&format!(
            "{prefix}{connector}{label}  tokens={}/{} tool_calls={}\n",
            node.prompt_tokens,
            node.completion_tokens,
            node.tool_calls,
            label = node.label
        ));
        let extra_prefix = if is_last { "    " } else { "│   " };
        let child_prefix = format!("{prefix}{extra_prefix}");
        if let Some(a) = node.agent_type.as_deref() {
            out.push_str(&format!("{child_prefix}agent_type={a}\n"));
        }
        if let Some(t) = node.task.as_deref() {
            out.push_str(&format!("{child_prefix}task=\"{t}\"\n"));
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
pub fn run_tree(args: &cli_args::JournalTreeArgs) -> Result<(), String> {
    let session_id = journal_digest::resolve_session_for_digest(
        args.session_id.as_deref(),
        args.session.as_deref(),
    )?;
    let (root, total, skipped) = build_tree(&session_id)?;
    let format = args.format.trim().to_ascii_lowercase();
    match format.as_str() {
        "" | "text" | "txt" => {
            let rendered = render_text(&root);
            print!("{rendered}");
            if skipped > 0 {
                eprintln!(
                    "[journal tree] note: skipped {skipped}/{total} events (unattachable SubRunCompleted, etc.)"
                );
            }
            Ok(())
        }
        "json" => {
            let body = serde_json::json!({
                "schema_version": "astra-journal-tree-v1",
                "session_id": session_id,
                "total_events": total,
                "skipped_events": skipped,
                "root": root,
            });
            println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
            Ok(())
        }
        other => Err(format!("invalid --format '{other}' (expected text or json)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::JournalEvent;
    use serde_json::json;

    fn turn_event(ts: &str, prompt: u64, completion: u64, model: &str) -> JournalEvent {
        let raw = json!({
            "type": "turn",
            "ts": ts,
            "session_id": "sess-1",
            "turn": 1,
            "model": model,
            "tokens_in": prompt,
            "tokens_out": completion,
        });
        serde_json::from_value(raw).expect("valid turn event")
    }

    fn sub_started(ts: &str, run_id: &str, agent_type: &str, task: &str) -> JournalEvent {
        let raw = json!({
            "type": "delegation_sub_run_started",
            "ts": ts,
            "session_id": "sess-1",
            "model": "sonnet",
            "metadata": {
                "run_id": run_id,
                "agent_type": agent_type,
                "task": task,
            }
        });
        serde_json::from_value(raw).expect("valid sub-run start")
    }

    fn sub_completed(ts: &str, run_id: &str, prompt: u64, completion: u64, tools: u32) -> JournalEvent {
        let raw = json!({
            "type": "delegation_sub_run_completed",
            "ts": ts,
            "session_id": "sess-1",
            "tokens_in": prompt,
            "tokens_out": completion,
            "tool_count": tools,
            "metadata": { "run_id": run_id },
        });
        serde_json::from_value(raw).expect("valid sub-run complete")
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
    fn tree_attaches_sub_runs_by_run_id() {
        let events = vec![
            turn_event("2026-04-30T10:00:00Z", 1000, 100, "sonnet"),
            sub_started("2026-04-30T10:00:01Z", "run-A", "coder", "refactor X"),
            sub_started("2026-04-30T10:00:02Z", "run-B", "reviewer", "review X"),
            sub_completed("2026-04-30T10:00:10Z", "run-A", 2000, 300, 4),
            sub_completed("2026-04-30T10:00:12Z", "run-B", 500, 100, 1),
        ];
        let (root, _total, skipped) = fold_events_into_tree("sess-1", &events);
        assert_eq!(skipped, 0);
        assert_eq!(root.children.len(), 2);
        let a = root.children.iter().find(|n| n.id == "run-A").unwrap();
        assert_eq!(a.label, "coder");
        assert_eq!(a.prompt_tokens, 2000);
        assert_eq!(a.completion_tokens, 300);
        assert_eq!(a.tool_calls, 4);
        assert_eq!(a.task.as_deref(), Some("refactor X"));
        assert_eq!(a.ended_at.as_deref(), Some("2026-04-30T10:00:10Z"));
        let b = root.children.iter().find(|n| n.id == "run-B").unwrap();
        assert_eq!(b.label, "reviewer");
        // Root roll-up includes both children's tokens + root's own turn.
        assert_eq!(root.prompt_tokens, 1000 + 2000 + 500);
        assert_eq!(root.completion_tokens, 100 + 300 + 100);
        assert_eq!(root.tool_calls, 4 + 1);
    }

    #[test]
    fn tree_counts_skipped_for_unattachable_completions() {
        let events = vec![
            // Completed without a started — can't attach.
            sub_completed("2026-04-30T10:00:10Z", "run-ghost", 500, 100, 1),
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
            sub_started("2026-04-30T10:00:01Z", "run-A", "coder", "refactor"),
            sub_completed("2026-04-30T10:00:10Z", "run-A", 200, 30, 2),
        ];
        let (root, _, _) = fold_events_into_tree("sess-1", &events);
        let txt = render_text(&root);
        assert!(txt.contains("root [sess-1]"));
        assert!(txt.contains("└── coder"));
        assert!(txt.contains("agent_type=coder"));
        assert!(txt.contains("task=\"refactor\""));
        assert!(txt.contains("started=2026-04-30T10:00:01Z ended=2026-04-30T10:00:10Z"));
    }

    #[test]
    fn tree_root_exposes_self_tokens_separate_from_total() {
        // Root accumulates its own Turn tokens AND rolls up children.
        // Consumers need to distinguish "root's own work" from "total
        // including delegation". self_prompt_tokens / self_completion_tokens
        // must reflect ONLY the root's Turn/LlmRound events.
        let events = vec![
            turn_event("t1", 1000, 100, "sonnet"),
            sub_started("t2", "run-A", "coder", "task"),
            sub_completed("t3", "run-A", 2000, 300, 4),
        ];
        let (root, _, _) = fold_events_into_tree("sess-1", &events);
        // Total = root own + children
        assert_eq!(root.prompt_tokens, 3000, "total prompt = 1000 + 2000");
        assert_eq!(root.completion_tokens, 400, "total completion = 100 + 300");
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
    fn render_text_emits_two_children_with_branch_and_last_connectors() {
        let events = vec![
            sub_started("t1", "run-A", "coder", "task1"),
            sub_started("t2", "run-B", "reviewer", "task2"),
            sub_completed("t3", "run-A", 100, 10, 1),
            sub_completed("t4", "run-B", 50, 5, 0),
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
