use std::io::Write;

use astra_core::{DriftCause, EvidenceType};
use astra_services::session_restore::RestoredSession;
use astra_services::{ForkSessionOptions, fork_local_session, session_journal, session_workspace};
use astra_turn_core::decision_explainer::{DriftDetector, FocusDriftAnalysis};
use chrono::{DateTime, Utc};

use crate::cli::permission_manager::PermissionMode;
use crate::cli::session::session_restore_client;
use crate::cli::session::session_runtime;
use crate::cli::surface::agent_journal_event_surface::{
    project_agent_spawned, project_agent_terminated,
};
use crate::cli::surface::delegation_event_surface::{
    project_delegation_completed, project_delegation_retry, project_delegation_started,
    project_delegation_sub_run_completed, project_delegation_sub_run_started,
};
use crate::cli::surface::session_source_surface::session_source_surface;
use crate::cli::surface::session_workspace_status_surface::session_workspace_status_surface;
use crate::cli::tool_call_groups;
use crate::cli::{
    cli_config::cli_utils::{
        SessionResumePreflight, clear_profile_last_session_if_matches_or_warn,
        get_profile_and_token, normalize_model_override, persist_profile_last_session_or_warn,
        preflight_remote_resume_session,
    },
    durable_bridge,
    session::session_state::{ContinuationAnchor, SessionState},
    session::{session_continuation, session_projection, session_startup, session_state},
    slash::slash_stats,
    stream::stream_render,
    theme,
};
use astra_runtime::prompts;
use crossterm::style::Stylize;

/// `/home/foo/bar` → `~/bar` when under the user home dir (readability).
fn tilde_path(abs: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return abs.to_string();
    };
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    if abs == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    if let Some(rest) = abs.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    abs.to_string()
}

/// Short relative age from RFC3339 `updated_at` (for scan-friendly lists).
fn rel_updated_label(iso: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(iso).ok()?.with_timezone(&Utc);
    let secs = Utc::now().signed_duration_since(dt).num_seconds();
    let secs = secs.max(0);
    if secs < 60 {
        return Some("just now".to_string());
    }
    if secs < 3600 {
        return Some(format!("{}m ago", secs / 60));
    }
    if secs < 86_400 {
        return Some(format!("{}h ago", secs / 3600));
    }
    if secs < 86_400 * 7 {
        return Some(format!("{}d ago", secs / 86_400));
    }
    Some(format!("{}d ago", secs / 86_400))
}

fn format_u64_grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn turn_count_label(turns: u32) -> String {
    if turns == 1 {
        "1 turn".to_string()
    } else {
        format!("{turns} turns")
    }
}

fn permission_audit_summary(metadata: Option<&serde_json::Value>) -> String {
    let Some(meta) = metadata else {
        return "unknown".to_string();
    };
    let kind = meta
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let tool = meta
        .get("request_key")
        .and_then(|value| value.get("tool"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    match kind {
        "evaluated" => {
            let decision = meta
                .get("decision")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            format!("evaluated {tool} -> {decision}")
        }
        "resolved" => {
            let response = meta
                .get("response")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let mut summary = format!("resolved {tool} -> {response}");
            if let Some(scope) = meta.get("scope").and_then(serde_json::Value::as_str) {
                summary.push_str(&format!(" ({scope})"));
            }
            summary
        }
        "persisted" => {
            let target = meta
                .get("target")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let saved = meta
                .get("saved")
                .and_then(serde_json::Value::as_bool)
                .map(|value| if value { "saved" } else { "failed" })
                .unwrap_or("?");
            format!("persisted {target} rule ({saved})")
        }
        other => other.to_string(),
    }
}

#[derive(Debug, Clone)]
enum SessionWorkspaceState {
    Present(Box<session_workspace::WorkspaceMetadata>),
    Missing { journal_turns: u32 },
    Invalid { error: String, journal_turns: u32 },
}

impl SessionWorkspaceState {
    fn load(session_id: &str) -> Self {
        match session_workspace::read_workspace_optional(session_id) {
            Ok(Some(workspace)) => Self::Present(Box::new(workspace)),
            Ok(None) => Self::Missing {
                journal_turns: session_journal::count_turns(session_id),
            },
            Err(error) => Self::Invalid {
                error: error.to_string(),
                journal_turns: session_journal::count_turns(session_id),
            },
        }
    }

    fn metadata(&self) -> Option<&session_workspace::WorkspaceMetadata> {
        match self {
            Self::Present(workspace) => Some(workspace.as_ref()),
            Self::Missing { .. } | Self::Invalid { .. } => None,
        }
    }

    fn summary_hint(&self) -> String {
        match self {
            Self::Present(ws) => {
                let mut parts: Vec<String> = Vec::new();
                let cwd = tilde_path(ws.cwd.as_str());
                parts.push(ellipsize(&cwd, 56));
                match (&ws.git_branch, &ws.git_head) {
                    (Some(b), Some(h)) => parts.push(format!("{b} @ {h}")),
                    (Some(b), None) => parts.push(b.clone()),
                    (None, Some(h)) => parts.push(format!("@ {h}")),
                    (None, None) => {}
                }
                if ws.turn_count > 0 {
                    parts.push(turn_count_label(ws.turn_count));
                }
                let status = session_workspace_status_surface(ws.status.as_str());
                if !status.is_active() {
                    parts.push(status.label().to_string());
                }
                if ws
                    .last_persistence_error
                    .as_deref()
                    .map(str::trim)
                    .filter(|error| !error.is_empty())
                    .is_some()
                {
                    parts.push("persistence degraded".to_string());
                }
                if let Some(lbl) = rel_updated_label(ws.updated_at.as_str()) {
                    parts.push(lbl);
                }
                parts.join(" · ")
            }
            Self::Missing { journal_turns } => {
                if *journal_turns > 0 {
                    format!(
                        "workspace metadata missing · journal has {}",
                        turn_count_label(*journal_turns)
                    )
                } else {
                    "workspace metadata missing".to_string()
                }
            }
            Self::Invalid {
                error: _,
                journal_turns,
            } => {
                if *journal_turns > 0 {
                    format!(
                        "workspace metadata unreadable · journal has {}",
                        turn_count_label(*journal_turns)
                    )
                } else {
                    "workspace metadata unreadable".to_string()
                }
            }
        }
    }
}

/// One-line hint for session lists: cwd, git, turns (from `workspace.yaml` if present).
fn workspace_summary_line(sid: &str) -> String {
    SessionWorkspaceState::load(sid).summary_hint()
}

fn resume_persistence_warning(error: Option<&str>) -> Option<String> {
    error
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .map(|error| format!("Session persistence degraded: {}", ellipsize(error, 96)))
}

fn list_local_sessions_by_time(limit: usize) -> Result<Vec<String>, String> {
    session_journal::list_sessions_by_time(limit)
        .map_err(|error| format!("failed to scan local sessions: {error}"))
}

fn list_local_sessions() -> Result<Vec<String>, String> {
    session_journal::list_sessions()
        .map_err(|error| format!("failed to scan local sessions: {error}"))
}

#[derive(Debug)]
struct ResumableSessionCandidates {
    sessions: Vec<astra_services::session_restore::RestoredSession>,
    local_scan_error: Option<String>,
    cloud_scan_error: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionListEntry {
    sid: String,
    workspace: SessionWorkspaceState,
    hint: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SessionListFilterOptions {
    filter_active: bool,
    filter_completed: bool,
    filter_here: bool,
    filter_project: bool,
    search_term: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SessionListFilterOutcome {
    skipped_missing_workspace: usize,
    skipped_invalid_workspace: usize,
    project_filter_ignored: bool,
}

impl SessionListFilterOutcome {
    fn record_workspace_skip(&mut self, workspace: &SessionWorkspaceState) {
        match workspace {
            SessionWorkspaceState::Present(_) => {}
            SessionWorkspaceState::Missing { .. } => self.skipped_missing_workspace += 1,
            SessionWorkspaceState::Invalid { .. } => self.skipped_invalid_workspace += 1,
        }
    }

    fn workspace_filter_warning(&self) -> Option<String> {
        let total = self.skipped_missing_workspace + self.skipped_invalid_workspace;
        if total == 0 {
            return None;
        }

        let mut reasons = Vec::new();
        if self.skipped_missing_workspace > 0 {
            reasons.push(format!(
                "{} missing workspace metadata",
                self.skipped_missing_workspace
            ));
        }
        if self.skipped_invalid_workspace > 0 {
            reasons.push(format!(
                "{} unreadable workspace metadata",
                self.skipped_invalid_workspace
            ));
        }

        Some(format!(
            "Skipped {total} session(s) that could not be evaluated for workspace-based filters ({})",
            reasons.join(", ")
        ))
    }
}

fn build_session_list_entries(session_ids: &[String]) -> Vec<SessionListEntry> {
    session_ids
        .iter()
        .map(|sid| {
            let workspace = SessionWorkspaceState::load(sid);
            let hint = workspace.summary_hint();
            SessionListEntry {
                sid: sid.clone(),
                workspace,
                hint,
            }
        })
        .collect()
}

fn retain_entries_with_workspace_filter<F>(
    entries: &mut Vec<SessionListEntry>,
    outcome: &mut SessionListFilterOutcome,
    predicate: F,
) where
    F: Fn(&session_workspace::WorkspaceMetadata) -> bool,
{
    entries.retain(|entry| match entry.workspace.metadata() {
        Some(workspace) => predicate(workspace),
        None => {
            outcome.record_workspace_skip(&entry.workspace);
            false
        }
    });
}

fn filter_session_list_entries(
    entries: &mut Vec<SessionListEntry>,
    options: &SessionListFilterOptions,
    current_cwd: &str,
    current_git_root: Option<&str>,
) -> SessionListFilterOutcome {
    let mut outcome = SessionListFilterOutcome::default();

    if options.filter_active {
        retain_entries_with_workspace_filter(entries, &mut outcome, |workspace| {
            session_workspace_status_surface(workspace.status.as_str()).is_active()
        });
    }
    if options.filter_completed {
        retain_entries_with_workspace_filter(entries, &mut outcome, |workspace| {
            session_workspace_status_surface(workspace.status.as_str()).is_completed()
        });
    }
    if options.filter_here {
        retain_entries_with_workspace_filter(entries, &mut outcome, |workspace| {
            workspace.cwd == current_cwd
        });
    }
    if options.filter_project {
        if let Some(root) = current_git_root {
            retain_entries_with_workspace_filter(entries, &mut outcome, |workspace| {
                workspace.git_root.as_deref() == Some(root)
            });
        } else {
            outcome.project_filter_ignored = true;
        }
    }

    if let Some(term) = options.search_term.as_ref() {
        entries.retain(|entry| {
            if entry.sid.to_lowercase().starts_with(term) {
                return true;
            }
            if entry.hint.to_lowercase().contains(term) {
                return true;
            }
            if let Some(workspace) = entry.workspace.metadata() {
                if workspace.cwd.to_lowercase().contains(term) {
                    return true;
                }
                if let Some(branch) = workspace.git_branch.as_ref()
                    && branch.to_lowercase().contains(term)
                {
                    return true;
                }
                if let Some(summary) = workspace.summary.as_ref()
                    && summary.to_lowercase().contains(term)
                {
                    return true;
                }
            }
            false
        });
    }

    outcome
}

async fn load_resumable_session_candidates(
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    local_limit: usize,
) -> Result<ResumableSessionCandidates, String> {
    let (cloud_sessions, cloud_scan_error) =
        match session_restore_client::list_cloud_resumable_sessions(profile, api).await {
            Ok(sessions) => (sessions, None),
            Err(error) => (
                Vec::new(),
                Some(format!("failed to load cloud resumable sessions: {error}")),
            ),
        };
    let (local_ids, local_scan_error) = match list_local_sessions_by_time(local_limit) {
        Ok(ids) => (ids, None),
        Err(error) if !cloud_sessions.is_empty() => (Vec::new(), Some(error)),
        Err(error) => {
            let mut failures = vec![error];
            if let Some(cloud_error) = cloud_scan_error.clone() {
                failures.push(cloud_error);
            }
            return Err(failures.join(" | "));
        }
    };

    let mut merged: std::collections::HashMap<
        String,
        astra_services::session_restore::RestoredSession,
    > = std::collections::HashMap::new();

    for sid in &local_ids {
        merged.entry(sid.clone()).or_insert_with(|| {
            astra_services::session_restore::RestoredSession {
                session_id: sid.clone(),
                turn_count: session_journal::count_turns(sid),
                last_status: "local".to_string(),
                ..Default::default()
            }
        });
    }

    for session in cloud_sessions {
        merged.insert(session.session_id.clone(), session);
    }

    let mut sessions: Vec<_> = Vec::new();
    for sid in &local_ids {
        if let Some(session) = merged.remove(sid) {
            sessions.push(session);
        }
    }
    let mut cloud_only: Vec<_> = merged.into_values().collect();
    cloud_only.sort_by_key(|session| std::cmp::Reverse(session.turn_count));
    sessions.splice(0..0, cloud_only);
    sessions.retain(|session| session.turn_count > 0);

    Ok(ResumableSessionCandidates {
        sessions,
        local_scan_error,
        cloud_scan_error,
    })
}

/// Resolve parent session id and optional label for `/session fork`.
fn parse_fork_source(arg: &str, state: &SessionState) -> Result<(String, Option<String>), String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return state
            .session_id
            .clone()
            .filter(|s| !s.is_empty())
            .map(|s| Ok((s, None)))
            .unwrap_or_else(|| {
                Err(
                    "no active session — use `/session fork <parent_session_id> [label]`"
                        .to_string(),
                )
            });
    }
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let head = parts[0];
    let tail = if parts.len() > 1 {
        Some(parts[1..].join(" "))
    } else {
        None
    };
    match session_journal::resolve_session_id(head) {
        Ok(sid) => Ok((sid, tail)),
        Err(_) => state
            .session_id
            .clone()
            .filter(|s| !s.is_empty())
            .map(|sid| Ok((sid, Some(arg.to_string()))))
            .unwrap_or_else(|| {
                Err(format!(
                    "unknown session id or prefix '{head}' (and no active session to fork from)"
                ))
            }),
    }
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

/// Print persisted workspace context (`workspace.yaml`).
fn print_workspace_metadata(ws: &session_workspace::WorkspaceMetadata, sid: &str) {
    eprintln!("  {}", "— workspace (persisted) —".dim());
    eprintln!(
        "  {:<16} {}",
        "cwd:".dim(),
        tilde_path(ws.cwd.as_str()).as_str().magenta()
    );
    let git_line = match (&ws.git_branch, &ws.git_head) {
        (Some(b), Some(h)) => format!("{b} @ {h}"),
        (Some(b), None) => b.clone(),
        (None, Some(h)) => format!("(detached) @ {h}"),
        (None, None) => "(no git at session start)".to_string(),
    };
    eprintln!("  {:<16} {}", "git:".dim(), git_line.magenta());
    if let Some(ref root) = ws.git_root
        && root != &ws.cwd
    {
        eprintln!(
            "  {:<16} {}",
            "repo root:".dim(),
            tilde_path(root.as_str()).as_str().dim()
        );
    }
    if let Some(ref p) = ws.parent_session_id {
        eprintln!(
            "  {:<16} {}",
            "forked from:".dim(),
            format!("{p} (turn {} on parent)", ws.forked_at_turn.unwrap_or(0)).magenta()
        );
        if let Some(ref n) = ws.fork_note {
            eprintln!("  {:<16} {}", "fork note:".dim(), n.as_str().magenta());
        }
    }
    if let Some(ref c) = ws.correlation_id {
        eprintln!("  {:<16} {}", "correlation:".dim(), c.as_str().magenta());
    }
    if let Some(ref r) = ws.agent_role {
        eprintln!("  {:<16} {}", "agent role:".dim(), r.as_str().magenta());
    }
    let started = ws.created_at.get(..19).unwrap_or(ws.created_at.as_str());
    eprintln!("  {:<16} {}", "started:".dim(), started.magenta());
    let saved = ws.updated_at.get(..19).unwrap_or(ws.updated_at.as_str());
    let ago = rel_updated_label(ws.updated_at.as_str())
        .map(|a| format!(" · {a}"))
        .unwrap_or_default();
    eprintln!(
        "  {:<16} {}{}",
        "last saved:".dim(),
        saved.magenta(),
        ago.dim()
    );
    let status = session_workspace_status_surface(ws.status.as_str());
    eprintln!(
        "  {:<16} {} {}",
        "status:".dim(),
        status.icon(),
        status.label().magenta()
    );
    if let Some(error) = ws
        .last_persistence_error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        eprintln!(
            "  {:<16} {}",
            "persistence:".dim(),
            ellipsize(error, 96).yellow()
        );
    }
    if let Some(ref sum) = ws.summary {
        eprintln!("  {:<16} {}", "summary:".dim(), ellipsize(sum, 80).dim());
    }
    if ws.turn_count > 0 || ws.total_tokens_in > 0 || ws.total_tokens_out > 0 {
        eprintln!(
            "  {:<16} {} turns · {} prompt + {} completion tokens",
            "logged:".dim(),
            ws.turn_count.to_string().magenta(),
            format_u64_grouped(ws.total_tokens_in).as_str().magenta(),
            format_u64_grouped(ws.total_tokens_out).as_str().magenta(),
        );
    }
    if let Some(ref goal) = ws.plan_goal {
        eprintln!(
            "  {:<16} {}",
            "plan goal:".dim(),
            ellipsize(goal, 72).magenta()
        );
    }
    if ws.plan_execution_rounds > 0 {
        eprintln!(
            "  {:<16} {}",
            "plan rounds:".dim(),
            ws.plan_execution_rounds.to_string().magenta()
        );
    }
    if let Some(ref trace) = ws.last_context_trace {
        eprintln!(
            "  {:<16} {}",
            "last trace:".dim(),
            ellipsize(&trace.preview(), 96).dim()
        );
    }
    if !ws.checkpoints.is_empty() {
        let preview: Vec<String> = ws
            .checkpoints
            .iter()
            .take(6)
            .map(|t| format!("T{t}"))
            .collect();
        let joined = preview.join(", ");
        let tail = if ws.checkpoints.len() > 6 {
            format!(" … (+{} more)", ws.checkpoints.len() - 6)
        } else {
            String::new()
        };
        eprintln!(
            "  {:<16} {}{}",
            "checkpoints:".dim(),
            joined.magenta(),
            tail.dim()
        );
    }
    if let Ok(ws_path) = session_workspace::workspace_file_path(sid) {
        let ws_disp = ws_path.display().to_string();
        eprintln!(
            "  {:<16} {}",
            "workspace.yaml:".dim(),
            tilde_path(&ws_disp).as_str().dim()
        );
    }
    eprintln!();
}

/// Handle `/session list [--all|--active|--completed] [--here|--project] [search_term]`
///
/// Sorting:
/// - Default: most recent first (by mtime)
/// - Shows up to 20 sessions unless `--all` specified
///
/// Filtering:
/// - `--active`: only active sessions
/// - `--completed`: only completed sessions
/// - `--here`: only sessions from current directory
/// - `--project`: only sessions from current git project (any branch)
///
/// Search:
/// - Matches session ID prefix, cwd, git branch, or summary
fn handle_session_list(sub_arg: &str, state: &SessionState) {
    // Parse options
    let mut show_all = false;
    let mut options = SessionListFilterOptions::default();

    for part in sub_arg.split_whitespace() {
        match part {
            "--all" | "-a" => show_all = true,
            "--active" => options.filter_active = true,
            "--completed" | "--done" => options.filter_completed = true,
            "--here" | "--cwd" => options.filter_here = true,
            "--project" | "--repo" => options.filter_project = true,
            _ if !part.starts_with('-') => options.search_term = Some(part.to_lowercase()),
            other => {
                eprintln!("{}", format!("  Unknown option: {other}").red());
                eprintln!(
                    "  {}",
                    "Usage: /session list [--all|--active|--completed|--here|--project] [search]"
                        .dim()
                );
                return;
            }
        }
    }

    // Get current cwd and git root for filtering
    let current_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let current_git_root = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    // Load sessions by recency
    let limit = if show_all { 500 } else { 50 }; // scan more than display for filtering
    let sessions = match list_local_sessions_by_time(limit) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", format!("  ✗ {e}").red());
            return;
        }
    };

    if sessions.is_empty() {
        eprintln!("{}", "  No journal files yet.".dim());
        eprintln!(
            "  {}",
            "Chat once to create a session, or check ~/.astra/sessions.".dim()
        );
        return;
    }

    let mut entries = build_session_list_entries(&sessions);
    let filter_outcome = filter_session_list_entries(
        &mut entries,
        &options,
        &current_cwd,
        current_git_root.as_deref(),
    );
    if filter_outcome.project_filter_ignored {
        eprintln!(
            "  {}",
            "Not in a git repository — --project filter ignored.".yellow()
        );
    }

    // Limit display
    let display_limit = if show_all { entries.len() } else { 20 };
    let total = entries.len();
    let showing = total.min(display_limit);

    if entries.is_empty() {
        let mut filter_desc = Vec::new();
        if options.filter_active {
            filter_desc.push("active");
        }
        if options.filter_completed {
            filter_desc.push("completed");
        }
        if options.filter_here {
            filter_desc.push("this directory");
        }
        if options.filter_project {
            filter_desc.push("this project");
        }
        let desc = if filter_desc.is_empty() {
            String::new()
        } else {
            format!(" ({})", filter_desc.join(", "))
        };
        eprintln!("  {}", format!("No sessions match{desc}.").dim());
        if let Some(warning) = filter_outcome.workspace_filter_warning() {
            eprintln!("  {}", warning.yellow());
        }
        return;
    }

    // Display header
    eprintln!(
        "\n{}",
        "─── Session Journals ────────────────────────────"
            .bold()
            .magenta()
    );
    let sort_info = "sorted by recent";
    let filter_info = {
        let mut parts = Vec::new();
        if options.filter_active {
            parts.push("active only");
        }
        if options.filter_completed {
            parts.push("completed only");
        }
        if options.filter_here {
            parts.push("this dir");
        }
        if options.filter_project {
            parts.push("this project");
        }
        if let Some(ref t) = options.search_term {
            parts.push(t);
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" · filter: {}", parts.join(", "))
        }
    };
    eprintln!("  {}", format!("{sort_info}{filter_info}").dim());
    if let Some(warning) = filter_outcome.workspace_filter_warning() {
        eprintln!("  {}", warning.yellow());
    }

    // Display entries with numbered shortcuts
    let current = state.session_id.as_deref().unwrap_or("");
    let mut shortcuts: Vec<String> = Vec::new();
    for (idx, entry) in entries.iter().take(display_limit).enumerate() {
        let marker = if entry.sid == current {
            " ← current"
        } else {
            ""
        };
        // Show abbreviated session ID (first 8 chars) for cleaner display
        let sid_short = if entry.sid.len() > 8 {
            &entry.sid[..8]
        } else {
            &entry.sid
        };
        // Show numbered shortcut for first 9 entries
        let num_label = if idx < 9 {
            shortcuts.push(entry.sid.clone());
            format!("[{}] ", idx + 1)
        } else {
            "    ".to_string()
        };
        eprintln!(
            "  {}{}  {}{}",
            num_label.dim(),
            sid_short.magenta(),
            entry.hint.as_str().dim(),
            marker.green()
        );
    }

    // Store shortcuts for /session switch
    if !shortcuts.is_empty() {
        LAST_SESSION_LIST.with(|cell| {
            *cell.borrow_mut() = shortcuts;
        });
        eprintln!("  {}", "Tip: /session switch <N> to resume by number".dim());
    }

    // Summary
    if total > showing {
        eprintln!(
            "  {} sessions match (showing {}; use --all for more)",
            total.to_string().dim(),
            showing
        );
    } else {
        eprintln!(
            "  {} {}",
            total.to_string().dim(),
            if total == 1 { "session" } else { "sessions" }
        );
    }
    eprintln!();
}

// Thread-local storage for session list shortcuts
thread_local! {
    static LAST_SESSION_LIST: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Get session ID from shortcut number (1-indexed)
fn get_session_shortcut(num: usize) -> Option<String> {
    LAST_SESSION_LIST.with(|cell| cell.borrow().get(num.saturating_sub(1)).cloned())
}

fn resume_restore_hint(error: &str) -> &'static str {
    if error.contains("not found")
        || error.contains("no resumable workspace/checkpoint state")
        || error.contains("no longer exists on the server")
    {
        "Use /resume to see available sessions."
    } else {
        "Check connection with /diagnostics, or try a different session."
    }
}

async fn switch_session_into_state(
    session_id: &str,
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
) -> Result<(), String> {
    restore_session_into_state(session_id, profile, api, state).await
}

/// Handle `/session switch <N>` - quick switch to session by number from last list
async fn handle_session_switch(
    sub_arg: &str,
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
) {
    let arg = sub_arg.trim();

    if arg.is_empty() {
        eprintln!(
            "  {}",
            "Usage: /session switch <N> (use number from /session list)".dim()
        );
        return;
    }

    // Parse number
    let num: usize = match arg.parse() {
        Ok(n) if (1..=9).contains(&n) => n,
        _ => {
            eprintln!("{}", format!("  Invalid number: {arg} (use 1-9)").red());
            return;
        }
    };

    // Get session ID from shortcuts
    let session_id = match get_session_shortcut(num) {
        Some(sid) => sid,
        None => {
            eprintln!(
                "  {}",
                format!("No session at position {num}. Run /session list first.").yellow()
            );
            return;
        }
    };

    // Show preview and confirm
    let (ws, workspace_error) = match session_workspace::read_workspace_optional(&session_id) {
        Ok(workspace) => (workspace, None),
        Err(error) => (None, Some(error)),
    };

    let short_id = &session_id[..8.min(session_id.len())];
    let summary = ws
        .as_ref()
        .and_then(|w| w.summary.clone())
        .map(|s| {
            let truncated: String = s.chars().take(50).collect();
            if s.chars().count() > 50 {
                format!("{truncated}…")
            } else {
                truncated
            }
        })
        .unwrap_or_else(|| "(no summary)".to_string());

    let turns = ws
        .as_ref()
        .map(|w| w.turn_count)
        .unwrap_or_else(|| session_journal::count_turns(&session_id));

    eprintln!(
        "\n  {} {}  {}  {} turns",
        format!("[{num}]").magenta().bold(),
        short_id.magenta(),
        summary.dim(),
        turns
    );
    if let Some(error) = workspace_error.as_ref() {
        eprintln!(
            "  {}",
            format!("workspace.yaml is invalid: {error}").yellow()
        );
    }

    // Quick confirm
    eprint!("  {} ", "Switch to this session? [Y/n]:".bold());
    std::io::Write::flush(&mut std::io::stderr()).ok();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_ok() {
        let answer = input.trim().to_lowercase();
        if !answer.is_empty() && answer != "y" && answer != "yes" {
            eprintln!("{}", "  Cancelled.".dim());
            return;
        }
    } else {
        return;
    }

    if let Err(error) = switch_session_into_state(&session_id, profile, api, state).await {
        let hint = resume_restore_hint(&error);
        eprintln!("  {} {}", theme::icon_err(), error.red());
        eprintln!("{}", format!("  {hint}").dim());
        return;
    }

    eprintln!(
        "  {} Switched to session {} ({} turns loaded)",
        theme::icon_ok(),
        short_id.magenta(),
        state.turn
    );
}

pub(crate) fn resolve_journal_target_session(
    sub_arg: &str,
    state: &SessionState,
    _missing_active_msg: &str,
) -> Result<(String, bool), String> {
    if !sub_arg.is_empty() {
        let requested = sub_arg.trim();
        let resolved =
            session_journal::resolve_session_id(requested).map_err(|e| format!("  ✗ {e}"))?;
        Ok((resolved.clone(), resolved != requested))
    } else if let Some(ref sid) = state.session_id {
        Ok((sid.clone(), false))
    } else {
        // No active session — list local journals and let user pick
        let sessions = list_local_sessions_by_time(10).map_err(|error| format!("  ✗ {error}"))?;
        if sessions.is_empty() {
            return Err("  No sessions found. Start a conversation to create one.".to_string());
        }
        eprintln!(
            "\n{}",
            "─── Available Sessions ──────────────────────────"
                .bold()
                .magenta()
        );
        eprintln!(
            "  {}",
            "newest first · path / git / turns from workspace.yaml".dim()
        );
        let show = sessions.len().min(10);
        for (i, sid) in sessions.iter().take(show).enumerate() {
            let hint = workspace_summary_line(sid);
            eprintln!(
                "  {}  {}  {}",
                format!("[{}]", i + 1).magenta().bold(),
                sid.as_str().magenta(),
                hint.dim()
            );
        }
        eprintln!();
        eprint!("  {} ", "Select (number or Enter to cancel):".bold());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok()
            && let Ok(n) = input.trim().parse::<usize>()
            && n >= 1
            && n <= show
        {
            return Ok((sessions[n - 1].clone(), false));
        }
        Err("  Cancelled.".to_string())
    }
}

pub(crate) async fn handle_session_command(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
    _token: Option<&str>,
) {
    let (sub_cmd, sub_arg) = match arg.find(char::is_whitespace) {
        Some(pos) => (arg[..pos].trim(), arg[pos..].trim()),
        None => (arg.trim(), ""),
    };
    match sub_cmd {
        "" => {
            // Show session info + available subcommands
            let sid = state.session_id.as_deref().unwrap_or("none");
            let mdl = state.model.as_deref().unwrap_or("default");
            eprintln!(
                "\n{}",
                "─── Session ─────────────────────────────────────"
                    .bold()
                    .magenta()
            );
            eprintln!("  {:<16} {}", "session_id:".dim(), sid.magenta());
            let (persisted_ws, persisted_ws_error) = if sid != "none" {
                match session_workspace::read_workspace_optional(sid) {
                    Ok(workspace) => (workspace, None),
                    Err(error) => (None, Some(error)),
                }
            } else {
                (None, None)
            };
            if sid != "none" {
                if let Some(ref ws) = persisted_ws {
                    print_workspace_metadata(ws, sid);
                    if let Some(started_model) = ws.model.as_deref()
                        && started_model != mdl
                    {
                        eprintln!("  {:<16} {}", "started as:".dim(), started_model.dim());
                    }
                } else {
                    eprintln!(
                        "  {}",
                        "— no workspace.yaml yet (cwd/git after journal init) —".dim()
                    );
                    eprintln!();
                }
                if let Some(error) = persisted_ws_error.as_ref() {
                    eprintln!(
                        "  {}",
                        format!("workspace.yaml is invalid: {error}").yellow()
                    );
                }
            } else {
                eprintln!();
            }
            eprintln!("  {}", "— this REPL —".dim());
            eprintln!("  {:<16} {}", "model:".dim(), mdl.magenta());
            if let Some(ref ws) = persisted_ws {
                if ws.turn_count != state.turn {
                    eprintln!(
                        "  {:<16} {} repl · {} logged",
                        "turns:".dim(),
                        state.turn.to_string().magenta(),
                        ws.turn_count.to_string().magenta()
                    );
                } else {
                    eprintln!(
                        "  {:<16} {}",
                        "turns:".dim(),
                        state.turn.to_string().magenta()
                    );
                }
            } else {
                eprintln!(
                    "  {:<16} {}",
                    "turns:".dim(),
                    state.turn.to_string().magenta()
                );
            }
            eprintln!(
                "  {:<16} {}",
                "explain:".dim(),
                state.explain.to_string().magenta()
            );
            eprintln!(
                "  {:<16} {}",
                "run_id:".dim(),
                state.run_id.as_deref().unwrap_or("none").magenta()
            );
            if let Some(error) = state
                .session_persistence_error
                .as_deref()
                .map(str::trim)
                .filter(|error| !error.is_empty())
            {
                let persisted_error = persisted_ws
                    .as_ref()
                    .and_then(|ws| ws.last_persistence_error.as_deref())
                    .map(str::trim);
                if persisted_error != Some(error) {
                    eprintln!(
                        "  {:<16} {}",
                        "persistence:".dim(),
                        ellipsize(error, 96).yellow()
                    );
                }
            }
            if let Some(ref j) = state.journal {
                let jp = j.path().display().to_string();
                eprintln!(
                    "  {:<16} {}",
                    "journal:".dim(),
                    tilde_path(&jp).as_str().magenta()
                );
            }
            eprintln!();
            eprintln!(
                "  {}",
                "Subcommands: history · context · errors · export · list [--here|--project|--active|search] · fork · cleanup · verify · drift"
                    .dim()
            );
            eprintln!();
        }

        "fork" => {
            let (parent_id, label) = match parse_fork_source(sub_arg, state) {
                Ok(x) => x,
                Err(msg) => {
                    eprintln!("{}", msg.red());
                    return;
                }
            };
            match fork_session_into_state(&parent_id, label, api, profile, state).await {
                Ok(outcome) => {
                    eprintln!(
                        "  {} New session {} (fork of {})",
                        theme::icon_ok(),
                        outcome.new_session_id.as_str().magenta(),
                        parent_id.as_str().dim()
                    );
                    eprintln!(
                        "  {}",
                        format!(
                            "{} journal events copied (excl. session end/start)",
                            outcome.events_copied
                        )
                        .dim()
                    );
                    eprintln!(
                        "  {}",
                        "REPL context is now the forked session (same history; new cloud lineage)."
                            .dim()
                    );
                    if outcome.preserved_existing_child_task_board {
                        eprintln!(
                            "  {}",
                            "Forked child already has a task board; preserved child tasks and skipped copying the parent board."
                                .yellow()
                        );
                    }
                }
                Err(error) => eprintln!("{}", format!("  ✗ {error}").red()),
            }
        }
        "history" => {
            // Read journal for this session or a specified session
            let (target_sid, resolved_prefix) = match resolve_journal_target_session(
                sub_arg,
                state,
                "  No active session. Use /session history <session_id>.",
            ) {
                Ok(value) => value,
                Err(msg) => {
                    eprintln!("{msg}");
                    return;
                }
            };
            if resolved_prefix {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    sub_arg.magenta(),
                    target_sid.as_str().magenta()
                );
            }
            match session_journal::read_journal(&target_sid) {
                Ok(events) if events.is_empty() => {
                    eprintln!(
                        "{}",
                        format!("  No journal entries for session {target_sid}").dim()
                    );
                }
                Ok(events) => {
                    eprintln!(
                        "\n{}",
                        format!(
                            "─── Session Journal ({}) ─────────────────────",
                            &target_sid[..8.min(target_sid.len())]
                        )
                        .bold()
                    );
                    for evt in &events {
                        let ts_short = evt.ts.get(11..19).unwrap_or(&evt.ts);
                        match evt.event_type {
                            session_journal::JournalEventType::SessionStart => {
                                eprintln!(
                                    "  {} {} session started (model: {})",
                                    ts_short.dim(),
                                    "▶".green(),
                                    evt.model.as_deref().unwrap_or("default").magenta(),
                                );
                            }
                            session_journal::JournalEventType::Turn => {
                                let input_preview: String = evt
                                    .user_input
                                    .as_deref()
                                    .unwrap_or("")
                                    .chars()
                                    .take(60)
                                    .collect();
                                eprintln!(
                                    "  {} {} T{} {} ({}ms, {}+{} tokens, {} tools)",
                                    ts_short.dim(),
                                    "●".magenta(),
                                    evt.turn.unwrap_or(0),
                                    input_preview,
                                    evt.duration_ms.unwrap_or(0),
                                    evt.tokens_in.unwrap_or(0),
                                    evt.tokens_out.unwrap_or(0),
                                    evt.tool_count.unwrap_or(0),
                                );
                                // Show any failed tool calls for auditability
                                if let Some(calls) = &evt.tool_calls {
                                    for group in tool_call_groups::group_tool_calls(calls) {
                                        let displays = group
                                            .calls
                                            .iter()
                                            .map(|tc| {
                                                stream_render::format_tool_display_from_preview(
                                                    &tc.name,
                                                    tc.args_preview.as_deref(),
                                                )
                                            })
                                            .collect::<Vec<_>>()
                                            .join(", ");
                                        let mut scope = match group.round {
                                            Some(round) => format!("r{round}"),
                                            None => "r?".to_string(),
                                        };
                                        if let Some(batch_id) = group.batch_id {
                                            scope.push_str(&format!(" · {batch_id}"));
                                        }
                                        if group.parallel || group.calls.len() > 1 {
                                            scope.push_str(&format!(
                                                " · {} calls",
                                                group.calls.len()
                                            ));
                                        }
                                        eprintln!(
                                            "    {} {} {}",
                                            "↳".dim(),
                                            scope.dim(),
                                            ellipsize(&displays, 96).dim(),
                                        );
                                        for tc in group.calls.iter().copied().filter(|c| !c.ok) {
                                            let err_preview = tc
                                                .error
                                                .as_deref()
                                                .unwrap_or("(no details)")
                                                .chars()
                                                .take(80)
                                                .collect::<String>();
                                            eprintln!(
                                                "      {} {} ({}ms) {}",
                                                theme::icon_err(),
                                                stream_render::format_tool_display_from_preview(
                                                    &tc.name,
                                                    tc.args_preview.as_deref(),
                                                ),
                                                tc.ms,
                                                err_preview.dim(),
                                            );
                                        }
                                    }
                                }
                            }
                            session_journal::JournalEventType::TurnError => {
                                eprintln!(
                                    "  {} {} T{} error: {}",
                                    ts_short.dim(),
                                    theme::icon_err(),
                                    evt.turn.unwrap_or(0),
                                    evt.error.as_deref().unwrap_or("(no details)").red(),
                                );
                            }
                            session_journal::JournalEventType::Compact => {
                                let summary_note = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("compact_summary"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| {
                                        let preview: String = s.chars().take(80).collect();
                                        if s.chars().count() > 80 {
                                            format!(" · {preview}…")
                                        } else {
                                            format!(" · {preview}")
                                        }
                                    })
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} compacted {} turns ({} facts){}",
                                    ts_short.dim(),
                                    "⟳".yellow(),
                                    evt.turns_compacted.unwrap_or(0),
                                    evt.facts_stored.unwrap_or(0),
                                    summary_note.dim(),
                                );
                            }
                            session_journal::JournalEventType::ConfigChange => {
                                eprintln!(
                                    "  {} {} {} → {}",
                                    ts_short.dim(),
                                    "⚙".dim(),
                                    evt.config_key.as_deref().unwrap_or("?"),
                                    evt.config_value.as_deref().unwrap_or("?").magenta(),
                                );
                            }
                            session_journal::JournalEventType::Error => {
                                eprintln!(
                                    "  {} {} {}",
                                    ts_short.dim(),
                                    theme::icon_err(),
                                    evt.error.as_deref().unwrap_or("(no details)").red(),
                                );
                            }
                            session_journal::JournalEventType::SessionEnd => {
                                eprintln!(
                                    "  {} {} session ended ({} turns total)",
                                    ts_short.dim(),
                                    "■".dim(),
                                    evt.turn.unwrap_or(0),
                                );
                            }
                            session_journal::JournalEventType::StallDetected => {
                                eprintln!(
                                    "  {} {} T{} stall: {}",
                                    ts_short.dim(),
                                    theme::icon_warn(),
                                    evt.turn.unwrap_or(0),
                                    evt.stall_type.as_deref().unwrap_or("(no details)").yellow(),
                                );
                            }
                            session_journal::JournalEventType::Checkpoint => {
                                let summary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("summary"))
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("checkpoint");
                                eprintln!(
                                    "  {} {} T{} checkpoint: {}",
                                    ts_short.dim(),
                                    "📌".green(),
                                    evt.turn.unwrap_or(0),
                                    summary,
                                );
                            }
                            session_journal::JournalEventType::TurnGuardVerdict => {
                                let severity = evt.stall_type.as_deref().unwrap_or("info");
                                let icon = match severity {
                                    "critical" => "🛑",
                                    "warning" => "⚠",
                                    _ => "ℹ",
                                };
                                let details = evt
                                    .metadata
                                    .as_ref()
                                    .map(|m| {
                                        let avoid = m
                                            .get("avoid_tools")
                                            .and_then(|v| v.as_array())
                                            .map(|a| a.len())
                                            .unwrap_or(0);
                                        let inj = m
                                            .get("injections")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        let reason = m
                                            .get("avoid_reason_summary")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        if reason.is_empty() {
                                            format!("{inj} nudges, {avoid} tools restricted")
                                        } else {
                                            format!(
                                                "{inj} nudges, {avoid} tools restricted ({reason})"
                                            )
                                        }
                                    })
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} T{} verdict[{}]: {}",
                                    ts_short.dim(),
                                    icon.yellow(),
                                    evt.turn.unwrap_or(0),
                                    severity.yellow(),
                                    details,
                                );
                            }
                            session_journal::JournalEventType::TurnEvaluation => {
                                let metadata = evt.metadata.as_ref();
                                let source = metadata
                                    .and_then(|m| m.get("source"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("runtime");
                                let quality = metadata
                                    .and_then(|m| m.get("quality"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let confidence = metadata
                                    .and_then(|m| m.get("confidence"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let success = metadata
                                    .and_then(|m| m.get("success"))
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let signals = metadata
                                    .and_then(|m| m.get("signals"))
                                    .and_then(|v| v.as_array())
                                    .map(|signals| {
                                        signals
                                            .iter()
                                            .filter_map(|signal| {
                                                signal.get("kind").and_then(|kind| kind.as_str())
                                            })
                                            .take(2)
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .filter(|summary| !summary.is_empty())
                                    .map(|summary| format!(" [{summary}]"))
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} {}eval[{}]: q={:.2} conf={:.2}{}",
                                    ts_short.dim(),
                                    if success {
                                        "◎".green().to_string()
                                    } else {
                                        theme::icon_warn().to_string()
                                    },
                                    evt.turn.map(|turn| format!("T{turn} ")).unwrap_or_default(),
                                    source.dim(),
                                    quality,
                                    confidence,
                                    signals,
                                );
                            }
                            session_journal::JournalEventType::PlanProgress => {
                                let action = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("action"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("progress");
                                let subtask = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("subtask_title"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let pct = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("progress_pct"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let icon = match action {
                                    "started" => "▶",
                                    "completed" => "✓",
                                    "plan_complete" => "✓",
                                    "plan_paused" => "⏸",
                                    "skipped" => "⏭",
                                    _ => "·",
                                };
                                eprintln!(
                                    "  {} {} T{} plan {}: {} ({}%)",
                                    ts_short.dim(),
                                    icon.magenta(),
                                    evt.turn.unwrap_or(0),
                                    action,
                                    subtask,
                                    pct,
                                );
                            }
                            session_journal::JournalEventType::PlanEdit => {
                                let action = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("action"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("edit");
                                eprintln!(
                                    "  {} {} plan edit: {}",
                                    ts_short.dim(),
                                    "✏".magenta(),
                                    action,
                                );
                            }
                            session_journal::JournalEventType::PlanLifecycle => {
                                let summary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("summary"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("lifecycle");
                                eprintln!(
                                    "  {} {} plan: {}",
                                    ts_short.dim(),
                                    "📋".magenta(),
                                    summary,
                                );
                            }
                            session_journal::JournalEventType::TaskLifecycle => {
                                let summary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("summary"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("task_updated");
                                let task_id = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("detail"))
                                    .and_then(|v| v.get("task_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("task");
                                eprintln!(
                                    "  {} {} T{} {} ({})",
                                    ts_short.dim(),
                                    "☑".cyan(),
                                    evt.turn.unwrap_or(0),
                                    summary,
                                    task_id,
                                );
                            }
                            session_journal::JournalEventType::GoalSteered => {
                                let source = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("source"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let new_goal = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("new_goal"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("updated goal");
                                if let Some(previous_goal) = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("previous_goal"))
                                    .and_then(|v| v.as_str())
                                {
                                    eprintln!(
                                        "  {} {} goal steered ({}): {} -> {}",
                                        ts_short.dim(),
                                        "🎯".magenta(),
                                        source,
                                        previous_goal,
                                        new_goal,
                                    );
                                } else {
                                    eprintln!(
                                        "  {} {} goal steered ({}): {}",
                                        ts_short.dim(),
                                        "🎯".magenta(),
                                        source,
                                        new_goal,
                                    );
                                }
                            }
                            session_journal::JournalEventType::ApprovalRequired => {
                                let approval =
                                    evt.metadata.as_ref().and_then(|m| m.get("approval"));
                                let tool = approval
                                    .and_then(|m| m.get("tool_name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let kind = approval
                                    .and_then(|m| m.get("approval_kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("standard");
                                eprintln!(
                                    "  {} {} approval required: {} ({})",
                                    ts_short.dim(),
                                    "⛔".yellow(),
                                    tool,
                                    kind,
                                );
                            }
                            session_journal::JournalEventType::ApprovalDecision => {
                                let approval =
                                    evt.metadata.as_ref().and_then(|m| m.get("approval"));
                                let tool = approval
                                    .and_then(|m| m.get("tool_name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let decision = approval
                                    .and_then(|m| m.get("decision"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let reason = approval
                                    .and_then(|m| m.get("reason"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if reason.is_empty() {
                                    eprintln!(
                                        "  {} {} approval decision: {} → {}",
                                        ts_short.dim(),
                                        theme::icon_ok(),
                                        tool,
                                        decision,
                                    );
                                } else {
                                    eprintln!(
                                        "  {} {} approval decision: {} → {} ({})",
                                        ts_short.dim(),
                                        theme::icon_ok(),
                                        tool,
                                        decision,
                                        reason.dim(),
                                    );
                                }
                            }
                            session_journal::JournalEventType::ApprovalTimeout => {
                                let approval =
                                    evt.metadata.as_ref().and_then(|m| m.get("approval"));
                                let tool = approval
                                    .and_then(|m| m.get("tool_name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                eprintln!(
                                    "  {} {} approval timed out: {}",
                                    ts_short.dim(),
                                    theme::icon_warn(),
                                    tool,
                                );
                            }
                            session_journal::JournalEventType::AskUserPrompted => {
                                let ask_user =
                                    evt.metadata.as_ref().and_then(|m| m.get("ask_user"));
                                let request_id = ask_user
                                    .and_then(|m| m.get("request_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let prompt = ask_user.and_then(|m| m.get("prompt"));
                                let question_count = prompt
                                    .and_then(|p| p.get("questions"))
                                    .and_then(|v| v.as_array())
                                    .map(|questions| questions.len())
                                    .unwrap_or(0);
                                let context = prompt
                                    .and_then(|p| p.get("context"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let context_suffix = if context.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · {}", ellipsize(context, 80).dim())
                                };
                                eprintln!(
                                    "  {} ? ask_user prompted: {} ({} questions){}",
                                    ts_short.dim(),
                                    ellipsize(request_id, 32),
                                    question_count,
                                    context_suffix,
                                );
                            }
                            session_journal::JournalEventType::AskUserResponse => {
                                let ask_user =
                                    evt.metadata.as_ref().and_then(|m| m.get("ask_user"));
                                let request_id = ask_user
                                    .and_then(|m| m.get("request_id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let status = ask_user
                                    .and_then(|m| m.get("status"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let answer_count = ask_user
                                    .and_then(|m| m.get("answers"))
                                    .and_then(|v| v.get("answers"))
                                    .and_then(|v| v.as_array())
                                    .map(|answers| answers.len())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} ask_user response: {} -> {} ({} answers)",
                                    ts_short.dim(),
                                    theme::icon_ok(),
                                    ellipsize(request_id, 32),
                                    status,
                                    answer_count,
                                );
                            }
                            session_journal::JournalEventType::PermissionAudit => {
                                eprintln!(
                                    "  {} {} permission audit: {}",
                                    ts_short.dim(),
                                    "🔐".cyan(),
                                    permission_audit_summary(evt.metadata.as_ref()),
                                );
                            }
                            session_journal::JournalEventType::ExecutionBoundaryOpened => {
                                let boundary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("execution_boundary"));
                                let kind = boundary
                                    .and_then(|m| m.get("kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("boundary");
                                let transaction_id = boundary
                                    .and_then(|m| m.get("transaction_id"))
                                    .and_then(|v| v.as_str());
                                let label = match (kind, transaction_id) {
                                    ("tool_batch", Some(id)) => format!("transaction `{id}`"),
                                    ("tool_batch", None) => "transaction".to_string(),
                                    ("turn_rollback", _) => "turn rollback".to_string(),
                                    (other, Some(id)) => format!("{other} `{id}`"),
                                    (other, None) => other.to_string(),
                                };
                                eprintln!(
                                    "  {} {} boundary opened: {}",
                                    ts_short.dim(),
                                    "⟦".magenta(),
                                    label,
                                );
                            }
                            session_journal::JournalEventType::ExecutionBoundaryCommitted => {
                                let boundary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("execution_boundary"));
                                let kind = boundary
                                    .and_then(|m| m.get("kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("boundary");
                                let transaction_id = boundary
                                    .and_then(|m| m.get("transaction_id"))
                                    .and_then(|v| v.as_str());
                                let label = match (kind, transaction_id) {
                                    ("tool_batch", Some(id)) => format!("transaction `{id}`"),
                                    ("tool_batch", None) => "transaction".to_string(),
                                    ("turn_rollback", _) => "turn rollback".to_string(),
                                    (other, Some(id)) => format!("{other} `{id}`"),
                                    (other, None) => other.to_string(),
                                };
                                eprintln!(
                                    "  {} {} boundary committed: {}",
                                    ts_short.dim(),
                                    theme::icon_ok(),
                                    label,
                                );
                            }
                            session_journal::JournalEventType::ExecutionBoundaryAborted => {
                                let boundary = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("execution_boundary"));
                                let kind = boundary
                                    .and_then(|m| m.get("kind"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("boundary");
                                let transaction_id = boundary
                                    .and_then(|m| m.get("transaction_id"))
                                    .and_then(|v| v.as_str());
                                let label = match (kind, transaction_id) {
                                    ("tool_batch", Some(id)) => format!("transaction `{id}`"),
                                    ("tool_batch", None) => "transaction".to_string(),
                                    ("turn_rollback", _) => "turn rollback".to_string(),
                                    (other, Some(id)) => format!("{other} `{id}`"),
                                    (other, None) => other.to_string(),
                                };
                                let reason = boundary
                                    .and_then(|m| m.get("reason"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("aborted");
                                eprintln!(
                                    "  {} {} boundary aborted: {} ({})",
                                    ts_short.dim(),
                                    theme::icon_warn(),
                                    label,
                                    reason.dim(),
                                );
                            }
                            session_journal::JournalEventType::SessionFork => {
                                let parent = evt
                                    .session_lineage
                                    .as_ref()
                                    .map(|l| l.parent_session_id.as_str())
                                    .unwrap_or("?");
                                let note = evt.user_input.as_deref().unwrap_or("");
                                eprintln!(
                                    "  {} {} fork ← {} {}",
                                    ts_short.dim(),
                                    "⎇".magenta(),
                                    parent.magenta(),
                                    note.dim()
                                );
                            }
                            session_journal::JournalEventType::SyncMarker => {
                                let ver = evt
                                    .edge_policy
                                    .as_ref()
                                    .and_then(|p| p.cloud_policy_version.as_deref())
                                    .unwrap_or("-");
                                let corr = evt
                                    .coordination
                                    .as_ref()
                                    .and_then(|c| c.correlation_id.as_deref())
                                    .unwrap_or("-");
                                let note = evt.user_input.as_deref().unwrap_or("");
                                eprintln!(
                                    "  {} {} sync policy:{} corr:{} {}",
                                    ts_short.dim(),
                                    "⇄".dim(),
                                    ver.dim(),
                                    corr.dim(),
                                    note.dim()
                                );
                            }
                            session_journal::JournalEventType::DelegationStarted => {
                                let projection = project_delegation_started(evt.metadata.as_ref());
                                eprintln!(
                                    "  {} {} delegation started ({}, {} agents)",
                                    ts_short.dim(),
                                    "⑂".magenta(),
                                    projection.pattern,
                                    projection.agent_count,
                                );
                            }
                            session_journal::JournalEventType::DelegationSubRunStarted => {
                                let projection =
                                    project_delegation_sub_run_started(evt.metadata.as_ref());
                                let retry_of = projection
                                    .retry_of
                                    .as_deref()
                                    .map(|run_id| format!(" (retry of {run_id})"))
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} sub-run {} started {}{}",
                                    ts_short.dim(),
                                    "↳".magenta(),
                                    projection.agent_id,
                                    projection.sub_run_id.dim(),
                                    retry_of.dim(),
                                );
                            }
                            session_journal::JournalEventType::DelegationSubRunCompleted => {
                                let projection =
                                    project_delegation_sub_run_completed(evt.metadata.as_ref());
                                let icon = crate::cli::surface::run_status_surface::run_status_icon(
                                    &projection.status,
                                );
                                eprintln!(
                                    "  {} {} sub-run {} → {}",
                                    ts_short.dim(),
                                    icon.magenta(),
                                    projection.agent_id,
                                    projection.status,
                                );
                                if let Some(preview) = projection
                                    .output_preview
                                    .as_deref()
                                    .filter(|s| !s.is_empty())
                                {
                                    eprintln!("      {}", ellipsize(preview, 120).dim());
                                }
                                if let Some(error) =
                                    projection.error.as_deref().filter(|s| !s.is_empty())
                                {
                                    eprintln!("      {}", ellipsize(error, 120).red());
                                }
                            }
                            session_journal::JournalEventType::DelegationCompleted => {
                                let projection =
                                    project_delegation_completed(evt.metadata.as_ref());
                                eprintln!(
                                    "  {} {} delegation done ({} ok, {} failed)",
                                    ts_short.dim(),
                                    "⑂".green(),
                                    projection.succeeded,
                                    projection.failed,
                                );
                                if let Some(preview) = projection
                                    .aggregated_output_preview
                                    .as_deref()
                                    .filter(|s| !s.is_empty())
                                {
                                    eprintln!("      {}", ellipsize(preview, 120).magenta());
                                }
                            }
                            session_journal::JournalEventType::AgentSpawned => {
                                let projection = project_agent_spawned(evt.metadata.as_ref());
                                eprintln!(
                                    "  {} {} agent spawned: {} ({})",
                                    ts_short.dim(),
                                    "┌".magenta(),
                                    projection.agent_id.magenta(),
                                    projection.description,
                                );
                            }
                            session_journal::JournalEventType::AgentTerminated => {
                                let projection = project_agent_terminated(evt.metadata.as_ref());
                                let turns = projection
                                    .turns_completed
                                    .map(|turns| turns.to_string())
                                    .unwrap_or_else(|| "not reported".to_string());
                                eprintln!(
                                    "  {} {} agent {} run {} → {} ({} turns)",
                                    ts_short.dim(),
                                    "⌁".dim(),
                                    projection.agent_id.dim(),
                                    projection.run_id.dim(),
                                    projection.status.magenta(),
                                    turns,
                                );
                            }
                            // Child conversation items have a dedicated
                            // transcript projection; printing every message in
                            // the session event timeline would duplicate and
                            // overwhelm lifecycle events.
                            session_journal::JournalEventType::TranscriptItem => {}
                            session_journal::JournalEventType::VerificationCompleted => {
                                let scope = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("scope"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("subtask");
                                let passed = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("passed"))
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let icon = if passed {
                                    theme::icon_ok()
                                } else {
                                    theme::icon_err()
                                };
                                eprintln!(
                                    "  {} {} {} verification {}",
                                    ts_short.dim(),
                                    icon,
                                    scope,
                                    if passed { "passed" } else { "failed" },
                                );
                            }
                            session_journal::JournalEventType::ContextAssemblyRecorded => {
                                // Context assembly trace (M1 telemetry) — detailed view via /session context
                                let tokens = evt
                                    .context_assembly_trace
                                    .as_ref()
                                    .and_then(|t| t.get("token_budget"))
                                    .and_then(|tb| tb.get("total_tokens_used"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} T{} context trace ({} tokens)",
                                    ts_short.dim(),
                                    "📊".magenta(),
                                    evt.turn.unwrap_or(0),
                                    tokens,
                                );
                            }
                            session_journal::JournalEventType::DelegationRetry => {
                                let projection = project_delegation_retry(evt.metadata.as_ref());
                                eprintln!(
                                    "  {} {} retry #{} {} → {} {}",
                                    ts_short.dim(),
                                    "↻".yellow(),
                                    projection.attempt,
                                    projection.original_run_id.dim(),
                                    projection.retry_run_id.dim(),
                                    projection.reason.dim(),
                                );
                            }
                            session_journal::JournalEventType::DriftDetected => {
                                let severity = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("severity"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let evidence_count = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("evidence_count"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} T{} drift detected (severity {:.2}, {} evidence)",
                                    ts_short.dim(),
                                    "↯".yellow(),
                                    evt.turn.unwrap_or(0),
                                    severity,
                                    evidence_count,
                                );
                            }
                            session_journal::JournalEventType::AdaptiveScenarioApplied => {
                                let scenario = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("scenario"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let confidence = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("confidence"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let n_changes = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("config_changes"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} {} T{} adaptive profile → {} (conf {:.2}, {} config changes)",
                                    ts_short.dim(),
                                    "⚙".magenta(),
                                    evt.turn.unwrap_or(0),
                                    scenario,
                                    confidence,
                                    n_changes,
                                );
                            }
                            session_journal::JournalEventType::AdaptivePerTurnApplied => {
                                let n_changes = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("changes"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                let triggers = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("triggers"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|t| t.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .unwrap_or_default();
                                if n_changes > 0 {
                                    eprintln!(
                                        "  {} {} T{} per-turn adapt: {} changes ({})",
                                        ts_short.dim(),
                                        "↻".magenta(),
                                        evt.turn.unwrap_or(0),
                                        n_changes,
                                        triggers,
                                    );
                                }
                            }
                            session_journal::JournalEventType::InterruptionRecorded => {
                                let interruption = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("interruption"))
                                    .cloned()
                                    .unwrap_or_else(|| serde_json::json!({}));
                                let kind = interruption
                                    .get("kind")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let resumable = interruption
                                    .get("resumable")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let tool_calls_completed = interruption
                                    .get("tool_calls_completed")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let turns_completed = interruption
                                    .get("turns_completed")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let remaining_turns = interruption
                                    .get("remaining_turns")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let step_suffix = evt
                                    .agentic_step
                                    .map(|step| format!(" step={step}"))
                                    .unwrap_or_default();
                                let icon = if resumable { "⏸" } else { "⛔" };
                                eprintln!(
                                    "  {} {} T{}{} interruption: {} (resumable={}, tools={}, turns={}, remaining={})",
                                    ts_short.dim(),
                                    icon.yellow(),
                                    evt.turn.unwrap_or(0),
                                    step_suffix.dim(),
                                    kind,
                                    resumable,
                                    tool_calls_completed,
                                    turns_completed,
                                    remaining_turns,
                                );
                            }
                            session_journal::JournalEventType::LlmRound => {
                                let meta = evt.metadata.as_ref();
                                let source = meta
                                    .and_then(|m| m.get("source"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("agentic_loop");
                                let finish_reason = meta
                                    .and_then(|m| m.get("finish_reason"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("-");
                                let step_suffix = evt
                                    .agentic_step
                                    .map(|step| format!(" step={step}"))
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} {} T{}{} r{} llm: {} (finish={}, tools={})",
                                    ts_short.dim(),
                                    "◌".dim(),
                                    evt.turn.unwrap_or(0),
                                    step_suffix.dim(),
                                    evt.round.unwrap_or(0),
                                    source,
                                    finish_reason,
                                    evt.tool_calls_returned.unwrap_or(0),
                                );
                            }
                            session_journal::JournalEventType::CompactionRetry => {
                                let retry_count = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("compaction"))
                                    .and_then(|v| v.get("retry_count"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let tokens_freed = evt
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.get("compaction"))
                                    .and_then(|v| v.get("tokens_freed"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                eprintln!(
                                    "  {} 🗜️ T{} compaction retry #{} (freed {} tokens)",
                                    ts_short.dim(),
                                    evt.turn.unwrap_or(0),
                                    retry_count,
                                    tokens_freed,
                                );
                            }
                            session_journal::JournalEventType::LlmRequestFull => {
                                eprintln!(
                                    "  {} 📥 T{} full LLM request captured",
                                    ts_short.dim(),
                                    evt.turn.unwrap_or(0),
                                );
                            }
                            session_journal::JournalEventType::LlmResponseFull => {
                                eprintln!(
                                    "  {} 📤 T{} full LLM response captured",
                                    ts_short.dim(),
                                    evt.turn.unwrap_or(0),
                                );
                            }
                            session_journal::JournalEventType::SessionMemoryExtraction => {
                                let meta = evt.metadata.as_ref();
                                let outcome = meta
                                    .and_then(|m| m.get("outcome"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let detail = meta
                                    .and_then(|m| {
                                        m.get("source")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string())
                                            .or_else(|| {
                                                m.get("reason")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string())
                                            })
                                    })
                                    .unwrap_or_default();
                                eprintln!(
                                    "  {} 📝 T{} session memory: {} ({})",
                                    ts_short.dim(),
                                    evt.turn.unwrap_or(0),
                                    outcome,
                                    detail,
                                );
                            }
                            session_journal::JournalEventType::PipelineFeedback
                            | session_journal::JournalEventType::PipelineAlert
                            | session_journal::JournalEventType::PipelineCompactionAudit
                            | session_journal::JournalEventType::Bootstrap
                            | session_journal::JournalEventType::TraceSpan
                            | session_journal::JournalEventType::ToolCallError => {
                                // Rendered by /inspect; suppress in timeline for now
                            }
                        }
                    }
                    // Summary stats
                    let turns: Vec<_> = events
                        .iter()
                        .filter(|e| e.event_type == session_journal::JournalEventType::Turn)
                        .collect();
                    let errors: Vec<_> = events
                        .iter()
                        .filter(|e| e.event_type == session_journal::JournalEventType::TurnError)
                        .collect();
                    let approval_required = events
                        .iter()
                        .filter(|e| {
                            e.event_type == session_journal::JournalEventType::ApprovalRequired
                        })
                        .count();
                    let approval_decisions = events
                        .iter()
                        .filter(|e| {
                            e.event_type == session_journal::JournalEventType::ApprovalDecision
                        })
                        .count();
                    let approval_timeouts = events
                        .iter()
                        .filter(|e| {
                            e.event_type == session_journal::JournalEventType::ApprovalTimeout
                        })
                        .count();
                    let ask_user_prompted = events
                        .iter()
                        .filter(|e| {
                            e.event_type == session_journal::JournalEventType::AskUserPrompted
                        })
                        .count();
                    let ask_user_responses = events
                        .iter()
                        .filter(|e| {
                            e.event_type == session_journal::JournalEventType::AskUserResponse
                        })
                        .count();
                    let boundary_opened = events
                        .iter()
                        .filter(|e| {
                            e.event_type
                                == session_journal::JournalEventType::ExecutionBoundaryOpened
                        })
                        .count();
                    let boundary_committed = events
                        .iter()
                        .filter(|e| {
                            e.event_type
                                == session_journal::JournalEventType::ExecutionBoundaryCommitted
                        })
                        .count();
                    let boundary_aborted = events
                        .iter()
                        .filter(|e| {
                            e.event_type
                                == session_journal::JournalEventType::ExecutionBoundaryAborted
                        })
                        .count();
                    let total_tokens_in: u64 = turns.iter().filter_map(|e| e.tokens_in).sum();
                    let total_tokens_out: u64 = turns.iter().filter_map(|e| e.tokens_out).sum();
                    let total_tools: u32 = turns.iter().filter_map(|e| e.tool_count).sum();
                    let total_ms: u64 = turns.iter().filter_map(|e| e.duration_ms).sum();
                    eprintln!(
                        "\n  {} {} turns, {} errors, {}+{} tokens, {} tool calls, {:.1}s total",
                        "Summary:".bold(),
                        turns.len(),
                        errors.len(),
                        total_tokens_in,
                        total_tokens_out,
                        total_tools,
                        total_ms as f64 / 1000.0,
                    );
                    if approval_required > 0 || approval_decisions > 0 || approval_timeouts > 0 {
                        eprintln!(
                            "  {} {} required, {} decisions, {} timeouts",
                            "Approvals:".bold(),
                            approval_required,
                            approval_decisions,
                            approval_timeouts,
                        );
                    }
                    if ask_user_prompted > 0 || ask_user_responses > 0 {
                        eprintln!(
                            "  {} {} prompted, {} responses",
                            "Ask user:".bold(),
                            ask_user_prompted,
                            ask_user_responses,
                        );
                    }
                    if boundary_opened > 0 || boundary_committed > 0 || boundary_aborted > 0 {
                        eprintln!(
                            "  {} {} opened, {} committed, {} aborted",
                            "Boundaries:".bold(),
                            boundary_opened,
                            boundary_committed,
                            boundary_aborted,
                        );
                    }
                    eprintln!();
                }
                Err(e) => {
                    eprintln!("{}", format!("  ✗ Failed to read journal: {e}").red());
                }
            }
        }
        "list" => {
            handle_session_list(sub_arg, state);
        }
        "context" => {
            // /session context [turn] [session_id]
            // Shows context assembly trace for a specific turn
            let parts: Vec<&str> = sub_arg.split_whitespace().collect();
            let (turn_str, session_arg) = match parts.len() {
                0 => (None, ""),
                1 => {
                    // Could be turn number or session id
                    if parts[0].parse::<u32>().is_ok() {
                        (Some(parts[0]), "")
                    } else {
                        (None, parts[0])
                    }
                }
                _ => (Some(parts[0]), parts[1]),
            };

            let (target_sid, resolved_prefix) =
                match resolve_journal_target_session(session_arg, state, "  No active session.") {
                    Ok(value) => value,
                    Err(msg) => {
                        eprintln!("{msg}");
                        return;
                    }
                };
            if resolved_prefix && !session_arg.is_empty() {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    session_arg.magenta(),
                    target_sid.as_str().magenta()
                );
            }

            match session_journal::read_journal(&target_sid) {
                Ok(events) => {
                    // Find context assembly traces
                    let traces: Vec<_> = events
                        .iter()
                        .filter(|e| {
                            e.event_type
                                == session_journal::JournalEventType::ContextAssemblyRecorded
                        })
                        .collect();

                    if traces.is_empty() {
                        eprintln!(
                            "  {} {}",
                            "ℹ".magenta(),
                            "No context traces in this session. Enable telemetry to record traces."
                                .dim()
                        );
                        return;
                    }

                    // If no turn specified, show summary of all traces
                    let turn_filter: Option<u32> = turn_str.and_then(|s| s.parse().ok());

                    if let Some(turn) = turn_filter {
                        // Show detailed trace for specific turn
                        let trace_evt = traces.iter().find(|e| e.turn == Some(turn));

                        match trace_evt {
                            Some(evt) => {
                                print_context_trace_detail(evt, turn);
                            }
                            None => {
                                eprintln!("  {} No context trace for turn {}", "✗".red(), turn);
                                let available: Vec<_> =
                                    traces.iter().filter_map(|e| e.turn).collect();
                                if !available.is_empty() {
                                    eprintln!(
                                        "  {} Available turns: {:?}",
                                        "ℹ".magenta(),
                                        available
                                    );
                                }
                            }
                        }
                    } else {
                        // No turn specified — show summary of all traces
                        eprintln!(
                            "\n{}",
                            format!(
                                "─── Context Traces ({}) ─────────────────────────",
                                traces.len()
                            )
                            .bold()
                            .magenta()
                        );
                        for evt in &traces {
                            let ts_short = evt.ts.get(11..19).unwrap_or(&evt.ts);
                            let t = evt.turn.unwrap_or(0);
                            let trace = evt.context_assembly_trace.as_ref();

                            let tokens_used = trace
                                .and_then(|t| t.get("token_budget"))
                                .and_then(|tb| tb.get("total_tokens_used"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let tools_count = trace
                                .and_then(|t| t.get("tools"))
                                .and_then(|t| t.get("visible_tools"))
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let memory_count = trace
                                .and_then(|t| t.get("memory"))
                                .and_then(|m| m.get("memories_selected"))
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);

                            eprintln!(
                                "  {} T{:<3} {} tokens · {} tools · {} memories",
                                ts_short.dim(),
                                t,
                                format_u64_grouped(tokens_used).magenta(),
                                tools_count,
                                memory_count,
                            );
                        }
                        eprintln!();
                        eprintln!(
                            "  {}",
                            "Use /session context <turn> to see full trace".dim()
                        );
                        eprintln!();
                    }
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
        "errors" => {
            let (target_sid, resolved_prefix) =
                match resolve_journal_target_session(sub_arg, state, "  No active session.") {
                    Ok(value) => value,
                    Err(msg) => {
                        eprintln!("{msg}");
                        return;
                    }
                };
            if resolved_prefix {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    sub_arg.magenta(),
                    target_sid.as_str().magenta()
                );
            }
            match session_journal::read_journal(&target_sid) {
                Ok(events) => {
                    let errors: Vec<_> = events
                        .iter()
                        .filter(|e| {
                            matches!(
                                e.event_type,
                                session_journal::JournalEventType::TurnError
                                    | session_journal::JournalEventType::Error
                            )
                        })
                        .collect();
                    if errors.is_empty() {
                        eprintln!(
                            "  {} {}",
                            theme::icon_ok(),
                            "No errors in this session.".green()
                        );
                    } else {
                        eprintln!(
                            "\n{}",
                            format!(
                                "─── Errors ({}) ─────────────────────────────────",
                                errors.len()
                            )
                            .bold()
                        );
                        for err in &errors {
                            let ts_short = err.ts.get(11..19).unwrap_or(&err.ts);
                            eprintln!(
                                "  {} T{} {}",
                                ts_short.dim(),
                                err.turn.unwrap_or(0),
                                err.error.as_deref().unwrap_or("?").red(),
                            );
                        }
                        eprintln!();
                    }
                }
                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
            }
        }
        "export" => {
            let (target_sid, resolved_prefix) =
                match resolve_journal_target_session(sub_arg, state, "  No active session.") {
                    Ok(value) => value,
                    Err(msg) => {
                        eprintln!("{msg}");
                        return;
                    }
                };
            if resolved_prefix {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    sub_arg.magenta(),
                    target_sid.as_str().magenta()
                );
            }
            export_session_markdown(&target_sid);
        }
        "cleanup" => {
            handle_session_cleanup(sub_arg, state);
        }
        "switch" | "sw" => {
            handle_session_switch(sub_arg, profile, api, state).await;
        }
        "verify" | "sync" | "status" => {
            handle_session_verify(state);
        }
        "drift" => {
            handle_session_drift(sub_arg, state);
        }
        "adaptive" | "profile" | "tuning" => {
            handle_session_adaptive(sub_arg, state);
        }
        "analyze" | "diag" => {
            handle_session_analyze(sub_arg, state);
        }
        other => {
            eprintln!("{}", format!("  Unknown subcommand: {other}").red());
            eprintln!(
                "  {}",
                "Usage: /session [list|switch|history|context|errors|export|fork|cleanup|verify|drift|adaptive|analyze] …"
                    .dim()
            );
        }
    }
}

// ── Context trace display ───────────────────────────────────────────────────

/// Print detailed context assembly trace for a specific turn.
fn print_context_trace_detail(evt: &session_journal::JournalEvent, turn: u32) {
    let trace = match evt.context_assembly_trace.as_ref() {
        Some(t) => t,
        None => {
            eprintln!("  {} No trace data for turn {}", "✗".red(), turn);
            return;
        }
    };

    eprintln!(
        "\n{}",
        format!("─── Context Assembly Trace (Turn {turn}) ─────────────────")
            .bold()
            .magenta()
    );

    // ─── Token Budget ───────────────────────────────────────────────────────
    if let Some(tb) = trace.get("token_budget") {
        eprintln!("\n  {}", "Token Budget".bold());
        let total = tb
            .get("total_tokens_used")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let limit = tb
            .get("context_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let pct = if limit > 0 {
            (total as f64 / limit as f64 * 100.0) as u32
        } else {
            0
        };
        eprintln!(
            "    {} / {} ({pct}%)",
            format_u64_grouped(total).magenta(),
            format_u64_grouped(limit).dim()
        );

        // Breakdown
        if let Some(breakdown) = tb.get("breakdown").and_then(|b| b.as_object()) {
            for (key, val) in breakdown {
                let tokens = val.as_u64().unwrap_or(0);
                if tokens > 0 {
                    eprintln!(
                        "      {:<20} {}",
                        key.as_str().dim(),
                        format_u64_grouped(tokens)
                    );
                }
            }
        }
    }

    // ─── System Prompt ──────────────────────────────────────────────────────
    if let Some(sp) = trace.get("system_prompt") {
        eprintln!("\n  {}", "System Prompt".bold());
        let total = sp.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        eprintln!("    Total: {} tokens", format_u64_grouped(total).magenta());

        let base = sp
            .get("base_persona_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let env = sp
            .get("environment_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if base > 0 {
            eprintln!(
                "      {:<20} {}",
                "base_persona".dim(),
                format_u64_grouped(base)
            );
        }
        if env > 0 {
            eprintln!(
                "      {:<20} {}",
                "environment".dim(),
                format_u64_grouped(env)
            );
        }

        // Skills
        if let Some(skills) = sp.get("skills_injected").and_then(|s| s.as_array()) {
            if !skills.is_empty() {
                eprintln!("    Skills:");
                for skill in skills.iter().take(5) {
                    let name = skill.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let tokens = skill.get("tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    eprintln!("      {} ({} tokens)", name.green(), tokens);
                }
                if skills.len() > 5 {
                    eprintln!("      … and {} more", skills.len() - 5);
                }
            }
        }

        // Memories
        if let Some(memories) = sp.get("repository_memories").and_then(|m| m.as_array()) {
            if !memories.is_empty() {
                eprintln!("    Repository Memories: {}", memories.len());
            }
        }
    }

    // ─── History Selection ──────────────────────────────────────────────────
    if let Some(hist) = trace.get("history") {
        eprintln!("\n  {}", "History Selection".bold());
        let total = hist
            .get("total_turns_available")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let retained = hist
            .get("turns_retained")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let compressed = hist
            .get("turns_compressed")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let dropped = hist
            .get("turns_dropped")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        eprintln!(
            "    {} available → {} retained, {} compressed, {} dropped",
            total,
            retained.to_string().green(),
            compressed.to_string().yellow(),
            dropped.to_string().red()
        );

        if let Some(ratio) = hist.get("compression_ratio").and_then(|v| v.as_f64()) {
            eprintln!("    Compression ratio: {:.1}%", ratio * 100.0);
        }
    }

    // ─── Memory Retrieval ───────────────────────────────────────────────────
    if let Some(mem) = trace.get("memory") {
        eprintln!("\n  {}", "Memory Retrieval".bold());
        if let Some(query) = mem.get("query").and_then(|v| v.as_str()) {
            let q_short = if query.len() > 60 {
                format!("{}…", &query[..query.floor_char_boundary(60)])
            } else {
                query.to_string()
            };
            eprintln!("    Query: \"{}\"", q_short.dim());
        }

        let candidates = mem
            .get("candidates_considered")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let selected = mem
            .get("memories_selected")
            .and_then(|m| m.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let total_tokens = mem
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        eprintln!(
            "    {} candidates → {} selected ({} tokens)",
            candidates,
            selected.to_string().green(),
            format_u64_grouped(total_tokens)
        );

        // Show top memories
        if let Some(memories) = mem.get("memories_selected").and_then(|m| m.as_array()) {
            for m in memories.iter().take(3) {
                let content = m
                    .get("content_preview")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let score = m
                    .get("relevance_score")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let short = if content.len() > 50 {
                    format!("{}…", &content[..content.floor_char_boundary(50)])
                } else {
                    content.to_string()
                };
                eprintln!("      [{:.2}] \"{}\"", score, short.dim());
            }
        }
    }

    // ─── tool surface ─────────────────────────────────────────────────────
    if let Some(tools) = trace.get("tools") {
        eprintln!("\n  {}", "tool surface".bold());
        let strategy = tools
            .get("strategy")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let confidence = tools
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let available = tools
            .get("tools_available")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        eprintln!(
            "    Strategy: {} (confidence: {:.2})",
            strategy.magenta(),
            confidence
        );

        if let Some(selected) = tools.get("visible_tools").and_then(|t| t.as_array()) {
            eprintln!(
                "    Selected ({}/{}): {}",
                selected.len(),
                available,
                selected
                    .iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .take(10)
                    .collect::<Vec<_>>()
                    .join(", ")
                    .green()
            );
        }

        // Budget stats
        let budget_used = tools
            .get("budget_used")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if budget_used > 0 {
            eprintln!(
                "    Tool schemas: {} tokens",
                format_u64_grouped(budget_used)
            );
        }
    }

    // ─── Explanations ───────────────────────────────────────────────────────
    if let Some(explanations) = trace.get("explanations").and_then(|e| e.as_array()) {
        if !explanations.is_empty() {
            eprintln!("\n  {}", "Decision Explanations".bold());
            for exp in explanations.iter().take(5) {
                let decision = exp
                    .get("decision_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let reasoning = exp.get("reasoning").and_then(|v| v.as_str()).unwrap_or("?");
                eprintln!("    {} {}", decision.magenta(), reasoning.dim());
            }
        }
    }

    eprintln!();
}

// ── Session export ──────────────────────────────────────────────────────────

/// Format tool call records as a summarized markdown block.
fn format_tool_calls_md(calls: &[session_journal::ToolCallRecord]) -> String {
    let mut out = String::new();
    out.push_str("\n<details>\n<summary>Tool calls</summary>\n\n");
    for group in tool_call_groups::group_tool_calls(calls) {
        let mut header = match group.round {
            Some(round) => format!("Round {round}"),
            None => "Round ?".to_string(),
        };
        if let Some(batch_id) = group.batch_id {
            header.push_str(&format!(" · batch {batch_id}"));
        }
        if group.parallel || group.calls.len() > 1 {
            header.push_str(&format!(" · {} parallel calls", group.calls.len()));
        }
        out.push_str(&format!("- **{header}**\n"));
        for tc in &group.calls {
            let status = if tc.ok { "✓" } else { "✗" };
            let display = stream_render::format_tool_display_from_preview(
                &tc.name,
                tc.args_preview.as_deref(),
            );
            out.push_str(&format!("  - `{display}` {status} ({}ms)\n", tc.ms));
            if let Some(ref err) = tc.error {
                out.push_str(&format!("    > Error: {err}\n"));
            }
            if let Some(ref preview) = tc.result_preview {
                let short = if preview.len() > 200 {
                    format!("{}…", &preview[..preview.floor_char_boundary(200)])
                } else {
                    preview.clone()
                };
                out.push_str(&format!(
                    "    > ```\n    > {}\n    > ```\n",
                    short.replace('\n', "\n    > ")
                ));
            }
        }
    }
    out.push_str("\n</details>\n\n");
    out
}

/// Build a markdown export from journal events.
pub(crate) fn build_export_markdown(
    session_id: &str,
    workspace: Option<&session_workspace::WorkspaceMetadata>,
    events: &[session_journal::JournalEvent],
) -> String {
    let mut md = format!("# Session: {session_id}\n\n");
    if let Some(error) = workspace
        .and_then(|workspace| workspace.last_persistence_error.as_deref())
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        md.push_str(&format!(
            "> Warning: Session persistence degraded: {error}\n\n"
        ));
    }
    for evt in events {
        let ts_short = evt.ts.get(..19).unwrap_or(&evt.ts);
        match evt.event_type {
            session_journal::JournalEventType::SessionStart => {
                md.push_str(&format!(
                    "## Session Start\n- **Time:** {ts_short}\n- **Model:** {}\n\n",
                    evt.model.as_deref().unwrap_or("default")
                ));
            }
            session_journal::JournalEventType::Turn => {
                md.push_str(&format!(
                    "### Turn {}\n- **Time:** {ts_short}\n- **Duration:** {}ms\n- **Tokens:** {} → {}\n- **Tools used:** {}\n\n",
                    evt.turn.unwrap_or(0),
                    evt.duration_ms.unwrap_or(0),
                    evt.tokens_in.unwrap_or(0),
                    evt.tokens_out.unwrap_or(0),
                    evt.tool_count.unwrap_or(0),
                ));

                if let Some(ref input) = evt.user_input {
                    if !input.is_empty() {
                        md.push_str(&format!("**User:**\n\n{input}\n\n"));
                    }
                }

                // Tool call details (collapsed)
                if let Some(ref calls) = evt.tool_calls {
                    if !calls.is_empty() {
                        md.push_str(&format_tool_calls_md(calls));
                    }
                }

                if let Some(ref output) = evt.assistant_output {
                    if !output.is_empty() {
                        md.push_str(&format!("**Assistant:**\n\n{output}\n\n"));
                    }
                }
                md.push_str("---\n\n");
            }
            session_journal::JournalEventType::TurnError => {
                md.push_str(&format!(
                    "### Turn {} ❌ Error\n- **Time:** {ts_short}\n- **Error:** {}\n\n---\n\n",
                    evt.turn.unwrap_or(0),
                    evt.error.as_deref().unwrap_or("(no details)"),
                ));
            }
            session_journal::JournalEventType::Compact => {
                let summary_line = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("compact_summary"))
                    .and_then(|v| v.as_str())
                    .map(|s| format!("- **Summary:** {s}\n"))
                    .unwrap_or_default();
                md.push_str(&format!(
                    "### Compact\n- **Time:** {ts_short}\n- **Turns compacted:** {}\n- **Facts stored:** {}\n{summary_line}\n",
                    evt.turns_compacted.unwrap_or(0),
                    evt.facts_stored.unwrap_or(0),
                ));
            }
            session_journal::JournalEventType::ConfigChange => {
                md.push_str(&format!(
                    "- ⚙️ {ts_short}: {} → {}\n",
                    evt.config_key.as_deref().unwrap_or("?"),
                    evt.config_value.as_deref().unwrap_or("?"),
                ));
            }
            session_journal::JournalEventType::SessionEnd => {
                md.push_str(&format!(
                    "## Session End\n- **Time:** {ts_short}\n- **Total turns:** {}\n",
                    evt.turn.unwrap_or(0),
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryOpened => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                md.push_str(&format!(
                    "### Execution boundary opened\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n\n"
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryCommitted => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                md.push_str(&format!(
                    "### Execution boundary committed\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n\n"
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryAborted => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                let reason = boundary
                    .and_then(|m| m.get("reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("aborted");
                md.push_str(&format!(
                    "### Execution boundary aborted\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n- **Reason:** {reason}\n\n"
                ));
            }
            session_journal::JournalEventType::SessionFork => {
                let parent = evt
                    .session_lineage
                    .as_ref()
                    .map(|l| l.parent_session_id.as_str())
                    .unwrap_or("?");
                md.push_str(&format!(
                    "### Session fork\n- **Time:** {ts_short}\n- **Parent:** {parent}\n- **Note:** {}\n\n",
                    evt.user_input.as_deref().unwrap_or(""),
                ));
            }
            session_journal::JournalEventType::SyncMarker => {
                md.push_str(&format!(
                    "### Sync marker\n- **Time:** {ts_short}\n- **Note:** {}\n\n",
                    evt.user_input.as_deref().unwrap_or(""),
                ));
            }
            session_journal::JournalEventType::AdaptiveScenarioApplied => {
                let meta = evt.metadata.as_ref();
                let scenario = meta
                    .and_then(|m| m.get("scenario"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let confidence = meta
                    .and_then(|m| m.get("confidence"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                md.push_str(&format!(
                    "### Adaptive scenario applied\n- **Time:** {ts_short}\n- **Scenario:** {scenario} (confidence {confidence:.2})\n\n"
                ));
            }
            session_journal::JournalEventType::AdaptivePerTurnApplied => {
                let n = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("changes"))
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if n > 0 {
                    md.push_str(&format!(
                        "### Per-turn adaptation (T{})\n- **Time:** {ts_short}\n- **Changes:** {n}\n\n",
                        evt.turn.unwrap_or(0),
                    ));
                }
            }
            session_journal::JournalEventType::AskUserPrompted => {
                let ask_user = evt.metadata.as_ref().and_then(|m| m.get("ask_user"));
                let request_id = ask_user
                    .and_then(|m| m.get("request_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let prompt = ask_user.and_then(|m| m.get("prompt"));
                let question_count = prompt
                    .and_then(|p| p.get("questions"))
                    .and_then(|v| v.as_array())
                    .map(|questions| questions.len())
                    .unwrap_or(0);
                let context = prompt
                    .and_then(|p| p.get("context"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("- **Context:** {s}\n"))
                    .unwrap_or_default();
                md.push_str(&format!(
                    "### Ask user prompted\n- **Time:** {ts_short}\n- **Request:** {request_id}\n- **Questions:** {question_count}\n{context}\n"
                ));
            }
            session_journal::JournalEventType::AskUserResponse => {
                let ask_user = evt.metadata.as_ref().and_then(|m| m.get("ask_user"));
                let request_id = ask_user
                    .and_then(|m| m.get("request_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let status = ask_user
                    .and_then(|m| m.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let answer_count = ask_user
                    .and_then(|m| m.get("answers"))
                    .and_then(|v| v.get("answers"))
                    .and_then(|v| v.as_array())
                    .map(|answers| answers.len())
                    .unwrap_or(0);
                md.push_str(&format!(
                    "### Ask user response\n- **Time:** {ts_short}\n- **Request:** {request_id}\n- **Status:** {status}\n- **Answers:** {answer_count}\n\n"
                ));
            }
            _ => {}
        }
    }
    md
}

/// Export a session journal to a timestamped Markdown file in the current directory.
fn export_session_markdown(session_id: &str) {
    match session_journal::read_journal(session_id) {
        Ok(events) if events.is_empty() => {
            eprintln!("{}", "  No journal entries to export.".dim());
        }
        Ok(events) => {
            let workspace = match session_workspace::read_workspace_optional(session_id) {
                Ok(workspace) => workspace,
                Err(error) => {
                    eprintln!(
                        "  {}",
                        format!(
                            "warning: workspace.yaml is invalid; export omits workspace health metadata: {error}"
                        )
                        .yellow()
                    );
                    None
                }
            };
            let md = build_export_markdown(session_id, workspace.as_ref(), &events);
            let now = chrono::Local::now();
            let export_path = format!("astra-session-{}.md", now.format("%Y%m%d-%H%M"));
            match std::fs::write(&export_path, &md) {
                Ok(_) => {
                    eprintln!(
                        "  {} Exported to {}",
                        theme::icon_ok(),
                        export_path.magenta()
                    )
                }
                Err(e) => eprintln!("{}", format!("  ✗ Failed to write: {e}").red()),
            }
        }
        Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
    }
}

// ── Session cleanup ─────────────────────────────────────────────────────────

/// Handle `/session cleanup [--days N] [--force] [--compress]`.
///
/// Default: show stale sessions (>30 days) and ask for confirmation.
/// `--days N` overrides the age threshold.
/// `--force` skips the confirmation prompt.
/// `--compress` archives completed journals to .jsonl.gz (instead of deleting).
fn handle_session_cleanup(arg: &str, state: &SessionState) {
    let tokens: Vec<&str> = arg.split_whitespace().collect();
    let mut max_days: u64 = 30;
    let mut force = false;
    let mut compress = false;

    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "--days" | "-d" => {
                if i + 1 < tokens.len() {
                    match tokens[i + 1].parse::<u64>() {
                        Ok(d) if d > 0 => max_days = d,
                        _ => {
                            eprintln!(
                                "{}",
                                format!("  ✗ Invalid --days value: {}", tokens[i + 1]).red()
                            );
                            return;
                        }
                    }
                    i += 2;
                } else {
                    eprintln!("{}", "  ✗ --days requires a number".red());
                    return;
                }
            }
            "--force" | "-f" => {
                force = true;
                i += 1;
            }
            "--compress" | "-c" => {
                compress = true;
                i += 1;
            }
            other => {
                eprintln!("{}", format!("  ✗ Unknown flag: {other}").red());
                eprintln!(
                    "  {}",
                    "Usage: /session cleanup [--days N] [--force] [--compress]".dim()
                );
                return;
            }
        }
    }

    // If --compress with no --days, compress all completed sessions (not just stale)
    if compress {
        handle_compress(state, force);
        return;
    }

    let max_age = std::time::Duration::from_secs(max_days * 86400);
    let current_sid = state.session_id.as_deref();

    let stale = match session_journal::find_stale_sessions(max_age, current_sid) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", format!("  ✗ Failed to scan sessions: {e}").red());
            return;
        }
    };

    if stale.is_empty() {
        eprintln!(
            "  {} No sessions older than {} days found.",
            theme::icon_ok(),
            max_days
        );
        return;
    }

    let total_bytes: u64 = stale.iter().map(|s| s.total_bytes).sum();
    eprintln!(
        "\n  Found {} session(s) older than {} days ({}):\n",
        stale.len().to_string().yellow(),
        max_days,
        human_bytes(total_bytes).yellow()
    );

    // Show at most 20 sessions in detail
    let show_count = stale.len().min(20);
    for info in &stale[..show_count] {
        let age = info
            .last_modified
            .elapsed()
            .map(|d| format_age_days(d))
            .unwrap_or_else(|_| "?".to_string());
        let short_id = if info.session_id.len() > 12 {
            &info.session_id[..12]
        } else {
            &info.session_id
        };
        eprintln!(
            "    {} {} turns, {}, {} ago",
            short_id.dim(),
            info.turns,
            human_bytes(info.total_bytes).dim(),
            age,
        );
    }
    if stale.len() > show_count {
        eprintln!(
            "    {}",
            format!("… and {} more", stale.len() - show_count).dim()
        );
    }
    eprintln!();

    if !force {
        eprint!("  Delete all {} sessions? [y/N] ", stale.len());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !input.trim().eq_ignore_ascii_case("y")
        {
            eprintln!("  {}", "Cancelled.".dim());
            return;
        }
    }

    let mut deleted = 0u64;
    let mut freed = 0u64;
    let mut errors = 0u64;
    for info in &stale {
        match session_journal::delete_session(&info.session_id) {
            Ok(bytes) => {
                deleted += 1;
                freed += bytes;
            }
            Err(e) => {
                errors += 1;
                eprintln!(
                    "    {} {}…: {}",
                    theme::icon_err(),
                    &info.session_id[..info.session_id.len().min(12)],
                    e.to_string().red()
                );
            }
        }
    }

    eprintln!(
        "  {} Deleted {} session(s), freed {}",
        theme::icon_ok(),
        deleted.to_string().green(),
        human_bytes(freed).green()
    );
    if errors > 0 {
        eprintln!(
            "  {} {} session(s) could not be deleted",
            theme::icon_warn(),
            errors
        );
    }
}

/// Compress completed session journals to .jsonl.gz.
fn handle_compress(state: &SessionState, force: bool) {
    let current_sid = state.session_id.as_deref();
    let archivable = match session_journal::find_archivable_sessions(current_sid) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{}", format!("  ✗ Failed to scan: {e}").red());
            return;
        }
    };

    if archivable.is_empty() {
        eprintln!("  {} No completed sessions to compress.", theme::icon_ok());
        return;
    }

    let total_bytes: u64 = archivable.iter().map(|(_, b)| b).sum();
    eprintln!(
        "\n  Found {} completed session(s) to compress ({}):\n",
        archivable.len().to_string().yellow(),
        human_bytes(total_bytes).yellow()
    );

    if !force {
        eprint!("  Compress {} session journals? [y/N] ", archivable.len());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err()
            || !input.trim().eq_ignore_ascii_case("y")
        {
            eprintln!("  {}", "Cancelled.".dim());
            return;
        }
    }

    let mut compressed = 0u64;
    let mut saved = 0u64;
    let mut errors = 0u64;
    for (sid, _) in &archivable {
        match session_journal::archive_journal(sid) {
            Ok((orig, comp)) => {
                compressed += 1;
                saved += orig.saturating_sub(comp);
            }
            Err(e) => {
                errors += 1;
                let short = &sid[..sid.len().min(12)];
                eprintln!(
                    "    {} {short}…: {}",
                    theme::icon_err(),
                    e.to_string().red()
                );
            }
        }
    }

    eprintln!(
        "  {} Compressed {} session(s), saved {}",
        theme::icon_ok(),
        compressed.to_string().green(),
        human_bytes(saved).green()
    );
    if errors > 0 {
        eprintln!(
            "  {} {} session(s) could not be compressed",
            theme::icon_warn(),
            errors
        );
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn format_age_days(d: std::time::Duration) -> String {
    let days = d.as_secs() / 86400;
    if days == 0 {
        "today".to_string()
    } else if days == 1 {
        "1 day".to_string()
    } else {
        format!("{days} days")
    }
}

// ── Session adaptive inspection ─────────────────────────────────────────────

/// Display current adaptive execution state: scenario, experiment, recent
/// per-turn adaptations, and tuning rule history from the session journal.
///
/// Usage: /session adaptive [--verbose]
fn handle_session_adaptive(_arg: &str, state: &SessionState) {
    use astra_services::session_journal;

    eprintln!(
        "\n{}",
        "─── Adaptive Execution State ─────────────────────"
            .bold()
            .magenta()
    );

    // 1. Current scenario + experiment from ObservabilitySession.
    if let Some(obs) = &state.observability_session {
        if let Ok(guard) = obs.read() {
            let scenario = guard
                .profile
                .current_scenario
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "none".to_string());
            eprintln!("  {} Scenario: {}", "▸".magenta(), scenario.bold());

            eprintln!("  {} Config snapshot:", "▸".magenta(),);
            eprintln!(
                "      token_budget.max_turn_input_tokens = {}",
                guard.config.token_budget.max_turn_input_tokens
            );
            eprintln!(
                "      memory.retrieval_top_k             = {}",
                guard.config.memory.retrieval_top_k
            );
            eprintln!(
                "      verification.strictness             = {:.3}",
                guard.config.verification.strictness
            );
            eprintln!(
                "      compression.compression_threshold   = {:.3}",
                guard.config.compression.compression_threshold
            );
        }
    } else {
        eprintln!("  {}", "No active observability session.".dim());
    }

    // 2. Recent adaptive events from journal.
    let session_id = match &state.session_id {
        Some(id) => id.to_string(),
        None => {
            eprintln!("\n  {}", "No active session for journal lookup.".dim());
            eprintln!();
            return;
        }
    };

    match session_journal::read_journal(&session_id) {
        Ok(events) => {
            let adaptive_events: Vec<_> = events
                .iter()
                .filter(|e| {
                    matches!(
                        e.event_type,
                        session_journal::JournalEventType::AdaptiveScenarioApplied
                            | session_journal::JournalEventType::AdaptivePerTurnApplied
                    )
                })
                .collect();

            if adaptive_events.is_empty() {
                eprintln!("\n  {}", "No adaptive events recorded yet.".dim());
            } else {
                eprintln!(
                    "\n  {} {} adaptive event(s) in journal:",
                    "▸".magenta(),
                    adaptive_events.len()
                );
                // Show last 10 events.
                let start = adaptive_events.len().saturating_sub(10);
                for evt in &adaptive_events[start..] {
                    let ts = chrono::DateTime::parse_from_rfc3339(&evt.ts)
                        .ok()
                        .map(|dt: chrono::DateTime<chrono::FixedOffset>| {
                            dt.format("%H:%M:%S").to_string()
                        })
                        .unwrap_or_else(|| "??:??:??".into());
                    let turn = evt.turn.unwrap_or(0);
                    match evt.event_type {
                        session_journal::JournalEventType::AdaptiveScenarioApplied => {
                            let scenario = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("scenario"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            let confidence = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("confidence"))
                                .and_then(|v| v.as_f64())
                                .unwrap_or(0.0);
                            let n = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("config_changes"))
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            eprintln!(
                                "    {} T{:>2} {} profile → {} (conf {:.2}, {} changes)",
                                ts.dim(),
                                turn,
                                "⚙".magenta(),
                                scenario,
                                confidence,
                                n
                            );
                        }
                        session_journal::JournalEventType::AdaptivePerTurnApplied => {
                            let n = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("changes"))
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let triggers = evt
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("triggers"))
                                .and_then(|v| v.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|t| t.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            eprintln!(
                                "    {} T{:>2} {} per-turn: {} changes ({})",
                                ts.dim(),
                                turn,
                                "↻".magenta(),
                                n,
                                triggers
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("  {}", format!("Failed to read journal: {e}").red());
        }
    }

    eprintln!();
}

// ── Session verify / sync status ────────────────────────────────────────────

/// Analyze and display focus drift in the current session.
///
/// Usage: /session drift [--verbose]
///
/// Uses the DriftDetector to analyze the conversation history for signs of
/// focus drift caused by compression, topic shifts, or user corrections.
fn handle_session_drift(arg: &str, state: &SessionState) {
    let verbose = arg.contains("--verbose") || arg.contains("-v");

    eprintln!(
        "\n{}",
        "─── Focus Drift Analysis ─────────────────────────"
            .bold()
            .magenta()
    );

    // Collect recent user queries from history (first element of each tuple)
    let user_queries: Vec<String> = state
        .history
        .iter()
        .map(|(user_msg, _assistant_msg)| user_msg.clone())
        .collect();

    if user_queries.is_empty() {
        eprintln!("  {} No conversation history yet.", theme::icon_ok());
        eprintln!();
        return;
    }

    // Build detector inputs
    let original_query = state
        .drift_original_query
        .as_deref()
        .unwrap_or_else(|| user_queries.first().map(|s| s.as_str()).unwrap_or(""));
    let compressed_turns: Vec<u32> = state.drift_compressed_turns.clone();
    let user_corrections: Vec<u32> = state.drift_user_corrections.clone();

    let analysis: FocusDriftAnalysis = {
        let detector = DriftDetector::default();
        detector.analyze(
            original_query,
            &user_queries,
            &compressed_turns,
            &user_corrections,
        )
    };

    // Display results
    if analysis.drift_detected {
        eprintln!(
            "  {} Focus drift detected (severity: {:.0}%)",
            theme::icon_warn(),
            analysis.drift_severity * 100.0
        );

        if let Some(turn) = analysis.drift_turn {
            eprintln!(
                "    Likely drift began at turn {}",
                turn.to_string().yellow()
            );
        }

        // Show cause
        let cause_str = match &analysis.likely_cause {
            DriftCause::HistoryCompression { lost_context, .. } => {
                let ctx = if lost_context.is_empty() {
                    "history".to_string()
                } else if lost_context.len() <= 3 {
                    lost_context.join(", ")
                } else {
                    format!(
                        "{} and {} more",
                        lost_context[..3].join(", "),
                        lost_context.len() - 3
                    )
                };
                format!("History compression (lost: {})", ctx)
            }
            DriftCause::MemoryMiss {
                expected_but_not_retrieved,
                ..
            } => {
                if expected_but_not_retrieved.is_empty() {
                    "Memory miss (expected memories not retrieved)".to_string()
                } else {
                    format!(
                        "Memory miss (expected: {})",
                        expected_but_not_retrieved.join(", ")
                    )
                }
            }
            DriftCause::TopicShift {
                original_topic,
                new_topic,
                ..
            } => {
                format!("Topic shift ('{}' → '{}')", original_topic, new_topic)
            }
            DriftCause::TokenBudgetPressure {
                budget_available,
                budget_needed,
                ..
            } => {
                format!(
                    "Token budget pressure ({} needed vs {} available)",
                    budget_needed, budget_available
                )
            }
            DriftCause::AmbiguousInstruction { instruction, .. } => {
                format!("Ambiguous instruction: {}", instruction)
            }
            DriftCause::Unknown => "Unknown cause".to_string(),
        };
        eprintln!("    Cause: {}", cause_str.yellow());

        // Show recovery suggestion
        if !analysis.recovery_suggestion.is_empty() {
            eprintln!("\n  💡 {}", analysis.recovery_suggestion.green());
        }
    } else {
        eprintln!(
            "  {} No significant focus drift detected.",
            theme::icon_ok()
        );
    }

    // Show tracked data if verbose
    if verbose {
        eprintln!("\n  {}", "Tracked Data".dim());
        eprintln!("    {:<22} {}", "History turns:".dim(), user_queries.len());
        eprintln!(
            "    {:<22} {}",
            "Compressed turns:".dim(),
            if compressed_turns.is_empty() {
                "none".to_string()
            } else {
                format!("{:?}", compressed_turns)
            }
        );
        eprintln!(
            "    {:<22} {}",
            "User corrections:".dim(),
            if user_corrections.is_empty() {
                "none".to_string()
            } else {
                format!("{:?}", user_corrections)
            }
        );
        eprintln!(
            "    {:<22} \"{}\"",
            "Original query:".dim(),
            ellipsize(original_query, 50)
        );

        // Show evidence
        if !analysis.evidence.is_empty() {
            eprintln!("\n  {}", "Evidence".dim());
            for ev in &analysis.evidence {
                // ev is DriftEvidence { turn, evidence_type, description, confidence }
                let type_str = match &ev.evidence_type {
                    EvidenceType::ToolCallTopicChange => "topic change",
                    EvidenceType::UserCorrection => "user correction",
                    EvidenceType::ClarificationRequest => "clarification",
                    EvidenceType::TermDisappearance => "term lost",
                    EvidenceType::CompressionLoss => "compression",
                    EvidenceType::MemoryMismatch => "memory miss",
                };
                eprintln!(
                    "    • Turn {}: [{}] {} ({:.0}%)",
                    ev.turn,
                    type_str,
                    ellipsize(&ev.description, 50),
                    ev.confidence.point * 100.0
                );
            }
        }
    }

    eprintln!();
}

// ── Session Analyze ─────────────────────────────────────────────────────────

/// `/session analyze [session_id]` — deep diagnostics for a session.
///
/// Reads the full journal + workspace and produces:
/// - Overview: model, duration, turns, total tokens
/// - Turn timeline with efficiency metrics
/// - Tool usage stats: frequency, success rate, blocked tools
/// - Token budget analysis: per-turn, cumulative, cache rate
/// - Issue detection: blocked tools, stalls, errors, latency spikes,
///   recording gaps, duplicate checkpoints
fn handle_session_analyze(arg: &str, state: &SessionState) {
    let (target_sid, resolved_prefix) = match resolve_journal_target_session(
        arg,
        state,
        "  No active session. Use /session analyze <session_id>.",
    ) {
        Ok(value) => value,
        Err(msg) => {
            eprintln!("{msg}");
            return;
        }
    };
    if resolved_prefix && !arg.is_empty() {
        eprintln!(
            "  {} Resolved {} → {}",
            theme::icon_ok(),
            arg.magenta(),
            target_sid.as_str().magenta()
        );
    }

    let events = match session_journal::read_journal(&target_sid) {
        Ok(e) if e.is_empty() => {
            eprintln!("{}", "  No journal entries.".dim());
            return;
        }
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", format!("  ✗ Failed to read journal: {e}").red());
            return;
        }
    };
    let (ws, workspace_error) = match session_workspace::read_workspace_optional(&target_sid) {
        Ok(workspace) => (workspace, None),
        Err(error) => (None, Some(error)),
    };

    // ── Collect turn events ─────────────────────────────────────────────────
    let turns: Vec<&session_journal::JournalEvent> = events
        .iter()
        .filter(|e| e.event_type == session_journal::JournalEventType::Turn)
        .collect();
    let errors: Vec<&session_journal::JournalEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e.event_type,
                session_journal::JournalEventType::TurnError
                    | session_journal::JournalEventType::Error
            )
        })
        .collect();
    let stalls: Vec<&session_journal::JournalEvent> = events
        .iter()
        .filter(|e| e.event_type == session_journal::JournalEventType::StallDetected)
        .collect();
    let checkpoints: Vec<&session_journal::JournalEvent> = events
        .iter()
        .filter(|e| e.event_type == session_journal::JournalEventType::Checkpoint)
        .collect();

    // ── Overview ────────────────────────────────────────────────────────────
    let sid_short = &target_sid[..8.min(target_sid.len())];
    eprintln!(
        "\n{}",
        format!("─── Session Analysis ({sid_short}) ──────────────────────────")
            .bold()
            .magenta()
    );

    let model = ws
        .as_ref()
        .and_then(|w| w.model.as_deref())
        .or_else(|| events.first().and_then(|e| e.model.as_deref()))
        .unwrap_or("unknown");
    let total_tok_in: u64 = turns.iter().filter_map(|t| t.tokens_in).sum();
    let total_tok_out: u64 = turns.iter().filter_map(|t| t.tokens_out).sum();
    let total_ms: u64 = turns.iter().filter_map(|t| t.duration_ms).sum();
    let total_tools: u32 = turns.iter().filter_map(|t| t.tool_count).sum();
    let total_cache_read: u64 = turns.iter().filter_map(|t| t.cache_read_tokens).sum();
    let total_cache_create: u64 = turns.iter().filter_map(|t| t.cache_creation_tokens).sum();

    eprintln!("  {:<16} {}", "model:".dim(), model.magenta());
    if let Some(error) = workspace_error.as_ref() {
        eprintln!(
            "  {:<16} {}",
            "workspace:".dim(),
            format!("invalid ({error})").yellow()
        );
    }
    eprintln!(
        "  {:<16} {} ({} prompt + {} completion)",
        "tokens:".dim(),
        format_u64_grouped(total_tok_in + total_tok_out).magenta(),
        format_u64_grouped(total_tok_in),
        format_u64_grouped(total_tok_out),
    );
    if total_cache_read > 0 || total_cache_create > 0 {
        let cache_pct = if total_tok_in > 0 {
            (total_cache_read as f64 / total_tok_in as f64 * 100.0) as u64
        } else {
            0
        };
        eprintln!(
            "  {:<16} {} read, {} created ({}% hit rate)",
            "cache:".dim(),
            format_u64_grouped(total_cache_read).green(),
            format_u64_grouped(total_cache_create),
            cache_pct,
        );
    }
    eprintln!(
        "  {:<16} {} turns, {} tool calls, {:.1}s total",
        "activity:".dim(),
        turns.len().to_string().magenta(),
        total_tools.to_string().magenta(),
        total_ms as f64 / 1000.0,
    );
    let _ = ws;

    // ── Turn Timeline ───────────────────────────────────────────────────────
    eprintln!(
        "\n{}",
        "  ── Turn Timeline ──────────────────────────────────────────".bold()
    );
    eprintln!(
        "  {:>4} {:>7} {:>8} {:>8} {:>5} {:>5}  {}",
        "Turn".dim(),
        "Time".dim(),
        "Tok-in".dim(),
        "Tok-out".dim(),
        "Tools".dim(),
        "Errs".dim(),
        "Input".dim(),
    );

    for evt in &turns {
        let turn_n = evt.turn.unwrap_or(0);
        let dur_s = evt.duration_ms.unwrap_or(0) as f64 / 1000.0;
        let tok_in = evt.tokens_in.unwrap_or(0);
        let tok_out = evt.tokens_out.unwrap_or(0);
        let tool_cnt = evt.tool_count.unwrap_or(0);

        // Count failed tool calls
        let err_cnt = evt
            .tool_calls
            .as_ref()
            .map(|calls| calls.iter().filter(|c| !c.ok).count())
            .unwrap_or(0);

        let input: String = evt
            .user_input
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();

        // Color-code by severity
        let dur_str = format!("{dur_s:>6.1}s");
        let dur_colored = if dur_s > 120.0 {
            dur_str.red().to_string()
        } else if dur_s > 60.0 {
            dur_str.yellow().to_string()
        } else {
            dur_str.to_string()
        };
        let tok_str = format!("{tok_in:>8}");
        let tok_colored = if tok_in > 80_000 {
            tok_str.red().to_string()
        } else if tok_in > 40_000 {
            tok_str.yellow().to_string()
        } else {
            tok_str.to_string()
        };
        let err_str = if err_cnt > 0 {
            format!("{err_cnt:>5}").red().to_string()
        } else {
            format!("{err_cnt:>5}")
        };

        eprintln!(
            "  {:>4} {} {} {:>8} {:>5} {}  {}",
            format!("T{turn_n}"),
            dur_colored,
            tok_colored,
            tok_out,
            tool_cnt,
            err_str,
            input.dim(),
        );
    }

    // ── Tool Usage ──────────────────────────────────────────────────────────
    let mut tool_stats: std::collections::HashMap<String, (u32, u32, u64, u64)> =
        std::collections::HashMap::new(); // name -> (total, fails, total_ms, total_output_bytes)
    let mut blocked_calls: Vec<(u32, String, String)> = Vec::new(); // (turn, tool, reason)
    let mut recording_gaps: Vec<(u32, u32, usize)> = Vec::new(); // (turn, reported_count, recorded_count)

    for evt in &turns {
        let turn_n = evt.turn.unwrap_or(0);
        let reported = evt.tool_count.unwrap_or(0);
        let recorded = evt.tool_calls.as_ref().map(|c| c.len()).unwrap_or(0);
        if reported > 0 && recorded > 0 && (reported as usize) > recorded + 2 {
            recording_gaps.push((turn_n, reported, recorded));
        }

        if let Some(calls) = evt.tool_calls.as_ref() {
            for call in calls {
                let entry = tool_stats.entry(call.name.clone()).or_insert((0, 0, 0, 0));
                entry.0 += 1;
                if !call.ok {
                    entry.1 += 1;
                }
                entry.2 += call.ms;
                entry.3 += call.output_bytes.unwrap_or(0) as u64;

                if let Some(ref err) = call.error {
                    if err.starts_with("blocked_tool:") {
                        let reason: String = err
                            .strip_prefix("blocked_tool: ")
                            .unwrap_or(err)
                            .chars()
                            .take(80)
                            .collect();
                        blocked_calls.push((turn_n, call.name.clone(), reason));
                    }
                }
            }
        }
    }

    if !tool_stats.is_empty() {
        eprintln!(
            "\n{}",
            "  ── Tool Usage ─────────────────────────────────────────────".bold()
        );
        eprintln!(
            "  {:>20} {:>5} {:>5} {:>7} {:>8}  {}",
            "Tool".dim(),
            "Total".dim(),
            "Fail".dim(),
            "Avg ms".dim(),
            "Output".dim(),
            "Rate".dim(),
        );

        let mut sorted_tools: Vec<_> = tool_stats.iter().collect();
        sorted_tools.sort_by_key(|x| std::cmp::Reverse(x.1.0));

        for (name, (total, fails, total_ms, total_bytes)) in &sorted_tools {
            let avg_ms = if *total > 0 {
                total_ms / *total as u64
            } else {
                0
            };
            let rate = if *total > 0 {
                ((*total - fails) as f64 / *total as f64 * 100.0) as u32
            } else {
                0
            };
            let rate_str = format!("{rate}%");
            let rate_colored = if rate < 50 {
                rate_str.red().to_string()
            } else if rate < 80 {
                rate_str.yellow().to_string()
            } else {
                rate_str.green().to_string()
            };
            let bytes_str = if *total_bytes > 1_000_000 {
                format!("{:.1}MB", *total_bytes as f64 / 1_000_000.0)
            } else if *total_bytes > 1_000 {
                format!("{:.1}KB", *total_bytes as f64 / 1_000.0)
            } else {
                format!("{}B", total_bytes)
            };
            let fail_str = if *fails > 0 {
                format!("{:>5}", fails).red().to_string()
            } else {
                format!("{:>5}", fails)
            };
            let name_short: String = name.chars().take(20).collect();
            eprintln!(
                "  {:>20} {:>5} {} {:>6}ms {:>8}  {}",
                name_short, total, fail_str, avg_ms, bytes_str, rate_colored,
            );
        }
    }

    // ── Issue Detection ─────────────────────────────────────────────────────
    let mut issues: Vec<String> = Vec::new();

    // Blocked tool calls
    if !blocked_calls.is_empty() {
        let mut by_tool: std::collections::HashMap<String, Vec<u32>> =
            std::collections::HashMap::new();
        for (turn, tool, _reason) in &blocked_calls {
            by_tool.entry(tool.clone()).or_default().push(*turn);
        }
        for (tool, turns_list) in &by_tool {
            let turns_str: Vec<String> = turns_list.iter().map(|t| format!("T{t}")).collect();
            issues.push(format!(
                "🚫 {} blocked {} time(s) in {}",
                tool.as_str().red(),
                turns_list.len(),
                turns_str.join(", "),
            ));
        }
    }

    // Recording gaps (skill opacity)
    for &(turn, reported, recorded) in &recording_gaps {
        issues.push(format!(
            "👁 T{turn}: {} tool calls reported but only {} recorded (skill opacity: {} invisible)",
            reported,
            recorded,
            reported as usize - recorded,
        ));
    }

    // Stalls
    for evt in &stalls {
        let turn = evt.turn.unwrap_or(0);
        let stype = evt.stall_type.as_deref().unwrap_or("unknown");
        issues.push(format!("⚠ T{turn}: stall detected ({stype})"));
    }

    // Errors
    for evt in &errors {
        let turn = evt.turn.unwrap_or(0);
        let err: String = evt
            .error
            .as_deref()
            .unwrap_or("?")
            .chars()
            .take(80)
            .collect();
        issues.push(format!("❌ T{turn}: {err}"));
    }

    // High latency turns (>120s)
    for evt in &turns {
        let turn = evt.turn.unwrap_or(0);
        let dur_s = evt.duration_ms.unwrap_or(0) as f64 / 1000.0;
        if dur_s > 120.0 {
            let tok = evt.tokens_in.unwrap_or(0);
            let tools = evt.tool_count.unwrap_or(0);
            issues.push(format!(
                "🐌 T{turn}: {dur_s:.0}s latency ({} prompt tokens, {tools} tool calls)",
                format_u64_grouped(tok),
            ));
        }
    }

    // Duplicate checkpoints (same turn, multiple checkpoints)
    let mut ckpt_by_turn: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for evt in &checkpoints {
        *ckpt_by_turn.entry(evt.turn.unwrap_or(0)).or_insert(0) += 1;
    }
    for (turn, count) in &ckpt_by_turn {
        if *count > 1 {
            issues.push(format!(
                "📌 T{turn}: {count} checkpoints (likely duplicated step_recorder + interval)"
            ));
        }
    }

    // High prompt token growth (>50K per turn after T1)
    let mut prev_tok_in: u64 = 0;
    for evt in &turns {
        let turn = evt.turn.unwrap_or(0);
        let tok_in = evt.tokens_in.unwrap_or(0);
        if turn > 1 && tok_in > prev_tok_in + 20_000 && tok_in > 60_000 {
            issues.push(format!(
                "📈 T{turn}: prompt tokens jumped to {} (+{})",
                format_u64_grouped(tok_in),
                format_u64_grouped(tok_in.saturating_sub(prev_tok_in)),
            ));
        }
        prev_tok_in = tok_in;
    }

    if !issues.is_empty() {
        eprintln!(
            "\n{}",
            format!(
                "  ── Issues ({}) ────────────────────────────────────────────",
                issues.len()
            )
            .bold()
            .yellow()
        );
        for issue in &issues {
            eprintln!("  {issue}");
        }
    } else {
        eprintln!("\n  {} {}", theme::icon_ok(), "No issues detected.".green());
    }

    // ── Efficiency Summary ──────────────────────────────────────────────────
    eprintln!(
        "\n{}",
        "  ── Efficiency ─────────────────────────────────────────────".bold()
    );

    let tok_per_tool = if total_tools > 0 {
        total_tok_in / total_tools as u64
    } else {
        0
    };
    eprintln!(
        "  {:<24} {} tokens/tool-call",
        "prompt efficiency:".dim(),
        format_u64_grouped(tok_per_tool),
    );

    let tok_per_turn = if !turns.is_empty() {
        total_tok_in / turns.len() as u64
    } else {
        0
    };
    eprintln!(
        "  {:<24} {} tokens/turn",
        "avg prompt per turn:".dim(),
        format_u64_grouped(tok_per_turn),
    );

    let avg_turn_ms = if !turns.is_empty() {
        total_ms / turns.len() as u64
    } else {
        0
    };
    eprintln!(
        "  {:<24} {:.1}s",
        "avg turn latency:".dim(),
        avg_turn_ms as f64 / 1000.0,
    );

    // Budget pressure distribution
    let pressures: Vec<f64> = turns.iter().filter_map(|e| e.budget_pressure).collect();
    if !pressures.is_empty() {
        let max_p = pressures.iter().cloned().fold(0.0_f64, f64::max);
        let avg_p = pressures.iter().sum::<f64>() / pressures.len() as f64;
        let pressure_str = format!("avg {avg_p:.2}, max {max_p:.2}");
        let pressure_colored = if max_p > 0.7 {
            pressure_str.red().to_string()
        } else if max_p > 0.4 {
            pressure_str.yellow().to_string()
        } else {
            pressure_str.green().to_string()
        };
        eprintln!("  {:<24} {}", "budget pressure:".dim(), pressure_colored,);
    }

    eprintln!();
}

/// Show local journal vs cloud ingestion sync health.
fn handle_session_verify(state: &SessionState) {
    let sid = state.session_id.as_deref().unwrap_or("none");
    eprintln!(
        "\n{}",
        "─── Sync Health ─────────────────────────────────"
            .bold()
            .magenta()
    );

    // Local journal stats
    let journal_events = if sid != "none" {
        session_journal::read_journal(sid)
            .map(|evts| evts.len())
            .unwrap_or(0)
    } else {
        0
    };
    let journal_path = if sid != "none" {
        let p = session_journal::journal_file_path(sid);
        if p.exists() {
            let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            format!(
                "{} ({})",
                tilde_path(&p.display().to_string()),
                human_bytes(bytes)
            )
        } else {
            "not found".to_string()
        }
    } else {
        "—".to_string()
    };

    eprintln!("  {}", "Local journal".dim());
    eprintln!(
        "    {:<20} {}",
        "events:".dim(),
        journal_events.to_string().magenta()
    );
    eprintln!("    {:<20} {}", "file:".dim(), journal_path.dim());

    eprintln!();
    eprintln!("  {}", "Cloud ingestion".dim());
    eprintln!(
        "    {}",
        "server-owned; CLI does not enqueue MatrixOne ingestion events".dim()
    );

    // Session disk usage summary
    if sid != "none" {
        let sessions_dir = session_journal::local_owner_sessions_dir();
        eprintln!();
        eprintln!("  {}", "Disk".dim());
        match list_local_sessions() {
            Ok(all_sessions) => {
                let total_journals: u64 = all_sessions
                    .iter()
                    .filter_map(|s| {
                        std::fs::metadata(session_journal::journal_file_path(s))
                            .ok()
                            .map(|m| m.len())
                    })
                    .sum();
                match std::fs::read_dir(&sessions_dir) {
                    Ok(entries) => {
                        let compressed = entries
                            .flatten()
                            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl.gz"))
                            .count();
                        eprintln!(
                            "    {:<20} {} active, {} archived",
                            "sessions:".dim(),
                            all_sessions.len().to_string().magenta(),
                            compressed.to_string().magenta()
                        );
                        eprintln!(
                            "    {:<20} {}",
                            "journal total:".dim(),
                            human_bytes(total_journals).magenta()
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "    {:<20} {}",
                            "sessions:".dim(),
                            format!("unavailable ({error})").yellow()
                        );
                        eprintln!(
                            "    {:<20} {}",
                            "journal total:".dim(),
                            format!("unavailable ({error})").yellow()
                        );
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "    {:<20} {}",
                    "sessions:".dim(),
                    format!("unavailable ({error})").yellow()
                );
                eprintln!(
                    "    {:<20} {}",
                    "journal total:".dim(),
                    format!("unavailable ({error})").yellow()
                );
            }
        }
    }

    eprintln!();
}

#[cfg(test)]
mod session_display_tests {
    use super::format_u64_grouped;

    #[test]
    fn format_u64_grouped_commas() {
        assert_eq!(format_u64_grouped(0), "0");
        assert_eq!(format_u64_grouped(999), "999");
        assert_eq!(format_u64_grouped(1000), "1,000");
        assert_eq!(format_u64_grouped(12_345_678), "12,345,678");
    }
}

#[cfg(test)]
mod export_tests {
    use super::{build_export_markdown, format_tool_calls_md};
    use astra_services::session_journal::{JournalEvent, ToolCallRecord};
    use astra_services::session_workspace;

    /// Construct a JournalEvent from a JSON value — avoids listing all fields.
    fn evt_from_json(json: serde_json::Value) -> JournalEvent {
        serde_json::from_value(json).expect("valid JournalEvent JSON")
    }

    #[test]
    fn build_export_includes_session_start() {
        let evt = evt_from_json(serde_json::json!({
            "type": "session_start",
            "ts": "2025-01-15T10:30:00Z",
            "model": "gpt-4o",
        }));
        let md = build_export_markdown("abc123", None, &[evt]);
        assert!(md.contains("# Session: abc123"));
        assert!(md.contains("## Session Start"));
        assert!(md.contains("gpt-4o"));
    }

    #[test]
    fn build_export_turn_with_tool_calls() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "duration_ms": 1500,
            "tokens_in": 100,
            "tokens_out": 50,
            "tool_count": 2,
            "user_input": "Hello",
            "assistant_output": "Hi there",
            "tool_calls": [
                {
                    "name": "read_file",
                    "ok": true,
                    "ms": 50,
                    "args_preview": "src/main.rs",
                    "result_preview": "fn main() { ... }",
                },
                {
                    "name": "bash",
                    "ok": false,
                    "ms": 200,
                    "error": "exit code 1",
                    "args_preview": "cargo test",
                },
            ],
        }));

        let md = build_export_markdown("test-sid", None, &[evt]);
        assert!(md.contains("### Turn 1"));
        assert!(md.contains("**User:**"));
        assert!(md.contains("Hello"));
        assert!(md.contains("<details>"));
        assert!(md.contains("`Reading: src/main.rs` ✓"));
        assert!(md.contains("`$ cargo test` ✗"));
        assert!(md.contains("exit code 1"));
        assert!(md.contains("**Assistant:**"));
        assert!(md.contains("Hi there"));
    }

    #[test]
    fn build_export_turn_without_tool_calls_omits_details() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "hi",
            "assistant_output": "hello",
        }));

        let md = build_export_markdown("sid", None, &[evt]);
        assert!(!md.contains("<details>"));
        assert!(md.contains("hello"));
    }

    #[test]
    fn format_tool_calls_md_produces_collapsed_block() {
        let calls = vec![ToolCallRecord {
            name: "grep".into(),
            ok: true,
            ms: 10,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("pattern in src/".into()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];
        let block = format_tool_calls_md(&calls);
        assert!(block.contains("<details>"));
        assert!(block.contains("</details>"));
        assert!(block.contains("Round ?"));
        assert!(block.contains("`Grep: pattern in src/` ✓ (10ms)"));
    }

    #[test]
    fn format_tool_calls_md_groups_parallel_batch() {
        let mut first = ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            ms: 11,
            args_preview: Some("src/lib.rs".into()),
            batch_id: Some("b-0-0".into()),
            parallel: Some(true),
            round: Some(0),
            ..Default::default()
        };
        first.result_preview = Some("mod app;".into());

        let second = ToolCallRecord {
            name: "grep".into(),
            ok: true,
            ms: 7,
            args_preview: Some("SessionState".into()),
            batch_id: Some("b-0-0".into()),
            parallel: Some(true),
            round: Some(0),
            ..Default::default()
        };

        let block = format_tool_calls_md(&[first, second]);
        assert!(block.contains("Round 0 · batch b-0-0 · 2 parallel calls"));
        assert!(block.contains("`Reading: src/lib.rs` ✓ (11ms)"));
        assert!(block.contains("`Grep: SessionState` ✓ (7ms)"));
    }

    // ── /export edge case tests ──

    #[test]
    fn build_export_empty_events_only_header() {
        let md = build_export_markdown("empty-sid", None, &[]);
        assert!(md.contains("# Session: empty-sid"));
        // Should not contain any section headers
        assert!(!md.contains("## Session Start"));
        assert!(!md.contains("### Turn"));
    }

    #[test]
    fn build_export_turn_error_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn_error",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 3,
            "error": "rate limit exceeded",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("Turn 3 ❌ Error"));
        assert!(md.contains("rate limit exceeded"));
    }

    #[test]
    fn build_export_compact_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "compact",
            "ts": "2025-01-15T10:35:00Z",
            "turns_compacted": 8,
            "facts_stored": 2,
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("### Compact"));
        assert!(md.contains("Turns compacted:** 8"));
        assert!(md.contains("Facts stored:** 2"));
    }

    #[test]
    fn build_export_config_change_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "config_change",
            "ts": "2025-01-15T10:33:00Z",
            "config_key": "model",
            "config_value": "gpt-4o-mini",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("⚙️"));
        assert!(md.contains("model → gpt-4o-mini"));
    }

    #[test]
    fn build_export_session_end_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "session_end",
            "ts": "2025-01-15T11:00:00Z",
            "turn": 15,
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("## Session End"));
        assert!(md.contains("Total turns:** 15"));
    }

    #[test]
    fn build_export_sync_marker_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "sync_marker",
            "ts": "2025-01-15T10:40:00Z",
            "user_input": "manual checkpoint",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("### Sync marker"));
        assert!(md.contains("manual checkpoint"));
    }

    #[test]
    fn build_export_non_ascii_content_preserved() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "请帮我修改代码 🔧",
            "assistant_output": "好的，我已经修改了。",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("请帮我修改代码 🔧"));
        assert!(md.contains("好的，我已经修改了。"));
    }

    #[test]
    fn build_export_multiple_event_types_in_order() {
        let events = vec![
            evt_from_json(serde_json::json!({
                "type": "session_start",
                "ts": "2025-01-15T10:30:00Z",
                "model": "gpt-4o",
            })),
            evt_from_json(serde_json::json!({
                "type": "turn",
                "ts": "2025-01-15T10:31:00Z",
                "turn": 1,
                "user_input": "hello",
                "assistant_output": "world",
            })),
            evt_from_json(serde_json::json!({
                "type": "turn_error",
                "ts": "2025-01-15T10:32:00Z",
                "turn": 2,
                "error": "timeout",
            })),
            evt_from_json(serde_json::json!({
                "type": "compact",
                "ts": "2025-01-15T10:33:00Z",
                "turns_compacted": 5,
                "facts_stored": 1,
            })),
            evt_from_json(serde_json::json!({
                "type": "session_end",
                "ts": "2025-01-15T11:00:00Z",
                "turn": 10,
            })),
        ];
        let md = build_export_markdown("multi", None, &events);
        // Check order: session_start before turns before session_end
        let start_pos = md.find("## Session Start").unwrap();
        let turn_pos = md.find("### Turn 1").unwrap();
        let error_pos = md.find("Turn 2 ❌").unwrap();
        let compact_pos = md.find("### Compact").unwrap();
        let end_pos = md.find("## Session End").unwrap();
        assert!(start_pos < turn_pos);
        assert!(turn_pos < error_pos);
        assert!(error_pos < compact_pos);
        assert!(compact_pos < end_pos);
    }

    #[test]
    fn build_export_turn_with_empty_user_input_omits_user() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "",
            "assistant_output": "some output",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(!md.contains("**User:**"));
        assert!(md.contains("**Assistant:**"));
    }

    #[test]
    fn build_export_ts_truncation() {
        // Timestamps longer than 19 chars are truncated
        let evt = evt_from_json(serde_json::json!({
            "type": "session_start",
            "ts": "2025-01-15T10:30:00.123456789Z",
            "model": "test",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("2025-01-15T10:30:00"));
        // Should not include the fractional seconds
        assert!(!md.contains(".123456789Z"));
    }

    #[test]
    fn build_export_header_surfaces_persistence_degradation() {
        let mut workspace = session_workspace::WorkspaceMetadata::new("sid", "gpt-5");
        workspace.last_persistence_error = Some("failed to append turn event".to_string());

        let md = build_export_markdown("sid", Some(&workspace), &[]);

        assert!(md.contains("Warning: Session persistence degraded"));
        assert!(md.contains("failed to append turn event"));
    }
}

// ═══════════════════════════════════════════════════════════ Resume ═══════

#[derive(Clone, Default)]
struct PreparedWorkspaceRestore {
    workspace: Option<session_workspace::WorkspaceMetadata>,
    session_persistence_error: Option<String>,
    discovered_skills: std::collections::HashSet<String>,
    pending_adaptive_state: Option<session_state::PersistedAdaptiveState>,
}

fn prepared_workspace_restore_from_workspace(
    ws: session_workspace::WorkspaceMetadata,
) -> PreparedWorkspaceRestore {
    let pending_adaptive_state = (ws.last_scenario_change_turn.is_some()
        || ws.last_token_budget_direction != 0
        || ws.active_experiment_id.is_some()
        || ws.tuned_config_json.is_some())
    .then(|| session_state::PersistedAdaptiveState {
        last_scenario_change_turn: ws.last_scenario_change_turn,
        last_token_budget_direction: ws.last_token_budget_direction,
        last_token_budget_change_turn: ws.last_token_budget_change_turn,
        active_experiment_id: ws.active_experiment_id.clone(),
        active_variant: ws.active_variant.clone(),
        tuned_config_json: ws.tuned_config_json.clone(),
    });
    PreparedWorkspaceRestore {
        session_persistence_error: ws.last_persistence_error.clone(),
        discovered_skills: ws.discovered_skills.iter().cloned().collect(),
        pending_adaptive_state,
        workspace: Some(ws),
    }
}

fn load_prepared_workspace_restore(
    restored: &RestoredSession,
) -> Result<PreparedWorkspaceRestore, String> {
    if let Some(workspace) = restored.workspace.clone() {
        return Ok(prepared_workspace_restore_from_workspace(workspace));
    }
    match session_workspace::read_workspace(&restored.session_id) {
        Ok(ws) => Ok(prepared_workspace_restore_from_workspace(ws)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(PreparedWorkspaceRestore::default())
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            let backup = session_workspace::backup_invalid_workspace_file(&restored.session_id)
                .map_err(|backup_error| {
                    format!(
                        "read workspace state for session {}: {e}; failed to move invalid workspace aside: {backup_error}",
                        restored.session_id
                    )
                })?;
            tracing::warn!(
                session_id = %restored.session_id,
                error = %e,
                backup_path = ?backup,
                "workspace metadata unreadable during resume; rebuilding from restored session"
            );
            let model_for_new = restored.model.as_deref().unwrap_or("default");
            let mut workspace =
                session_workspace::WorkspaceMetadata::new(&restored.session_id, model_for_new);
            workspace.last_persistence_error = Some(format!(
                "workspace metadata unreadable during resume; rebuilt from journal/checkpoint ({e})"
            ));
            Ok(prepared_workspace_restore_from_workspace(workspace))
        }
        Err(e) => Err(format!(
            "read workspace state for session {}: {e}",
            restored.session_id
        )),
    }
}

fn apply_prepared_workspace_restore(state: &mut SessionState, prepared: &PreparedWorkspaceRestore) {
    state.session_persistence_error = prepared.session_persistence_error.clone();
    state.discovered_skills = prepared.discovered_skills.clone();
    state.pending_adaptive_state = prepared.pending_adaptive_state.clone();
    session_startup::apply_pending_adaptive_state(state);
}

fn persist_resumed_workspace_metadata(
    restored: &RestoredSession,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    existing_workspace: Option<&session_workspace::WorkspaceMetadata>,
) -> Result<(), String> {
    let model_for_new = restored.model.as_deref().unwrap_or("default");
    let mut ws = existing_workspace
        .cloned()
        .or_else(|| restored.workspace.clone())
        .unwrap_or_else(|| {
            session_workspace::WorkspaceMetadata::new(&restored.session_id, model_for_new)
        });
    ws.turn_count = restored.turn_count;
    ws.total_tokens_in = restored.total_tokens_in;
    ws.total_tokens_out = restored.total_tokens_out;
    ws.total_cache_read_tokens = total_cache_read_tokens;
    ws.total_cache_creation_tokens = total_cache_creation_tokens;
    ws.status = restored.last_status.clone();
    if let Some(ref branch) = restored.git_branch {
        ws.git_branch = Some(branch.clone());
    }
    ws.model = astra_core::model_override::normalize_model_override_owned(restored.model.clone());
    if restored.permission_mode.is_some() {
        ws.permission_mode = restored.permission_mode.clone();
    }
    ws.executing_plan_json = restored.executing_plan_json.clone();
    ws.plan_goal = restored.plan_goal.clone();
    ws.plan_config_json = restored.plan_config_json.clone();
    ws.plan_execution_rounds = restored.plan_execution_rounds;
    ws.contract_json = restored.contract_json.clone();
    ws.plan_corrections = restored.plan_corrections.clone();
    ws.last_context_trace = restored.last_context_trace.clone();
    astra_services::session_workspace::write_workspace(&ws)
        .map_err(|e| format!("write workspace during resume: {e}"))
}

fn parse_restored_permission_mode(
    restored: &RestoredSession,
) -> Result<Option<PermissionMode>, String> {
    restored
        .permission_mode
        .as_deref()
        .map(str::parse::<PermissionMode>)
        .transpose()
        .map_err(|error| {
            format!(
                "Session {} has invalid persisted permission mode: {error}",
                restored.session_id
            )
        })
}

fn build_step_resume_guidance(
    interruption: Option<&serde_json::Value>,
    compaction_state: Option<&serde_json::Value>,
) -> Option<String> {
    let compaction_ctx = compaction_resume_context_from_checkpoint_state(compaction_state);
    interruption.and_then(|irj| {
        astra_turn_core::interruption::build_resume_guidance_with_context(
            irj,
            compaction_ctx.as_ref(),
        )
    })
}

fn compaction_resume_context_from_checkpoint_state(
    compaction_state: Option<&serde_json::Value>,
) -> Option<astra_turn_core::interruption::CompactionResumeContext> {
    compaction_state.map(
        |cs| astra_turn_core::interruption::CompactionResumeContext {
            compaction_attempts: cs
                .get("attempt_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            total_tokens_freed: cs
                .get("cumulative_tokens_freed")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            last_was_insufficient: cs
                .get("last_was_insufficient")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        },
    )
}

fn apply_resume_recovery_state(
    state: &mut SessionState,
    interruption: Option<&serde_json::Value>,
    compaction_state: Option<&serde_json::Value>,
) {
    state.last_turn_interrupted = interruption.is_some();
    state.resume_guidance = build_step_resume_guidance(interruption, compaction_state);
    state.resume_restricted_tools = interruption
        .map(astra_turn_core::interruption::resume_restricted_tools_from_interruption_json)
        .unwrap_or_default();
}

fn apply_runtime_recovery_state(
    state: &mut SessionState,
    pipeline_state: Option<&serde_json::Value>,
    compaction_state: Option<&serde_json::Value>,
    consecutive_context_window_errors: u32,
) {
    state.runtime_pipeline_state = pipeline_state.cloned();
    state.runtime_compaction_state = compaction_state.cloned();
    state.runtime_consecutive_context_window_errors = consecutive_context_window_errors;
}

/// Baseline row for a blocked tool when we have no persisted health metrics yet (same defaults as
/// cloud preference seeding in `cloud_sync.rs`).
fn blocked_tool_health_entry(
    name: String,
) -> astra_turn_core::tool_health_persistence::ToolHealthEntry {
    astra_turn_core::tool_health_persistence::ToolHealthEntry {
        name,
        total_calls: 0,
        total_failures: 0,
        failure_rate: 0.0,
        last_updated_epoch: 0,
        recent_outcomes: vec![],
    }
}

fn apply_heavy_state_fallback(
    state: &mut SessionState,
    blocked_tools: &[String],
    recent_tools: &[String],
    messages: &[serde_json::Value],
    approval_overrides: Option<&serde_json::Value>,
) {
    for tool in blocked_tools {
        if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
            state
                .tool_health_entries
                .push(blocked_tool_health_entry(tool.clone()));
        }
    }
    if let Some(ao_json) = approval_overrides {
        state.perm_manager.merge_restored_overrides(ao_json);
    }
    if state.recent_tools.is_empty() {
        state.recent_tools = recent_tools.to_vec();
    }
    if state.history.is_empty() {
        let pairs = session_continuation::history_pairs_from_messages(
            &session_continuation::sanitize_continuation_messages(messages.to_vec()),
        );
        if !pairs.is_empty() {
            state.history = pairs;
        }
    }
}

#[cfg(test)]
fn apply_heavy_checkpoint_fallback(
    state: &mut SessionState,
    heavy: &astra_pipeline::step_protocol::HeavyCheckpoint,
) {
    apply_heavy_state_fallback(
        state,
        &heavy.blocked_tools,
        &heavy.recent_tools,
        &heavy.messages,
        heavy.approval_overrides.as_ref(),
    );
}

fn apply_restored_cloud_heavy_state(state: &mut SessionState, restored: &RestoredSession) {
    apply_heavy_state_fallback(
        state,
        &restored.blocked_tools,
        &restored.recent_tools,
        &restored.conversation_messages,
        restored.approval_overrides.as_ref(),
    );
}

struct PreparedForkRestore {
    history: Vec<(String, String)>,
    recent_tools: Vec<String>,
    activated_deferred_tool_names: Vec<String>,
    csl_manager: Option<astra_turn_core::conversation_log::manager::CslManager>,
    journal_state: session_runtime::RestoredSessionState,
    last_turn_event: Option<session_journal::JournalEvent>,
}

#[derive(Debug, Clone)]
struct CloudTaskBoardCopy {
    cloud_base: String,
    token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkTaskBoardRestore {
    Copied,
    PreservedExistingChild,
}

struct ForkStateSnapshot {
    session_id: Option<String>,
    root_mailbox: Option<astra_messaging::router::AgentMailbox>,
    turn: u32,
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    last_turn_event: Option<session_journal::JournalEvent>,
    run_id: Option<String>,
    history: Vec<(String, String)>,
    recent_tools: Vec<String>,
    activated_deferred_tool_names: Vec<String>,
    csl_manager: Option<astra_turn_core::conversation_log::manager::CslManager>,
    last_response: Option<String>,
    continuation_anchor: Option<ContinuationAnchor>,
}

impl ForkStateSnapshot {
    fn capture(state: &mut SessionState) -> Self {
        Self {
            session_id: state.session_id.clone(),
            root_mailbox: state.root_mailbox.take(),
            turn: state.turn,
            total_prompt_tokens: state.total_prompt_tokens,
            total_completion_tokens: state.total_completion_tokens,
            total_cache_read_tokens: state.total_cache_read_tokens,
            total_cache_creation_tokens: state.total_cache_creation_tokens,
            last_turn_event: state.last_turn_event.clone(),
            run_id: state.run_id.clone(),
            history: state.history.clone(),
            recent_tools: state.recent_tools.clone(),
            activated_deferred_tool_names: state.activated_deferred_tool_names.clone(),
            csl_manager: state.csl_manager.take(),
            last_response: state.last_response.clone(),
            continuation_anchor: state.continuation_anchor.clone(),
        }
    }

    fn restore(self, state: &mut SessionState) {
        match self.session_id {
            Some(session_id) => {
                state.set_session_id(session_id.clone());
                session_startup::initialize_journal_pub(state, &session_id);
            }
            None => {
                state.clear_session_id();
                state.journal = None;
            }
        }
        state.turn = self.turn;
        state.total_prompt_tokens = self.total_prompt_tokens;
        state.total_completion_tokens = self.total_completion_tokens;
        state.total_cache_read_tokens = self.total_cache_read_tokens;
        state.total_cache_creation_tokens = self.total_cache_creation_tokens;
        state.last_turn_event = self.last_turn_event;
        state.run_id = self.run_id;
        state.history = self.history;
        state.recent_tools = self.recent_tools;
        state.activated_deferred_tool_names = self.activated_deferred_tool_names;
        state.csl_manager = self.csl_manager;
        state.root_mailbox = self.root_mailbox;
        state.last_response = self.last_response;
        state.continuation_anchor = self.continuation_anchor;
    }
}

struct ForkStateGuard<'a> {
    state: &'a mut SessionState,
    snapshot: Option<ForkStateSnapshot>,
}

impl<'a> ForkStateGuard<'a> {
    fn new(state: &'a mut SessionState) -> Self {
        Self {
            snapshot: Some(ForkStateSnapshot::capture(state)),
            state,
        }
    }

    fn state(&mut self) -> &mut SessionState {
        self.state
    }

    fn commit(mut self) {
        self.snapshot = None;
    }
}

impl Drop for ForkStateGuard<'_> {
    fn drop(&mut self) {
        if let Some(snapshot) = self.snapshot.take() {
            snapshot.restore(self.state);
        }
    }
}

fn materialize_prepared_fork_restore(
    mgr: astra_turn_core::conversation_log::manager::CslManager,
    mat: Option<astra_turn_core::conversation_log::MaterializedState>,
    restored_journal: session_runtime::RestoredJournalState,
) -> PreparedForkRestore {
    let mut history = restored_journal.session.history.clone();
    let mut recent_tools = restored_journal.session.recent_tools.clone();
    let mut activated_deferred_tool_names = Vec::new();
    if let Some(ref materialized) = mat {
        history = session_continuation::history_pairs_from_messages(
            &session_continuation::sanitize_continuation_messages(materialized.messages.clone()),
        );
        if !materialized.session_state.recent_tools.is_empty() {
            recent_tools = materialized.session_state.recent_tools.clone();
        }
        activated_deferred_tool_names = materialized
            .session_state
            .activated_deferred_tool_names
            .clone();
    }
    PreparedForkRestore {
        history,
        recent_tools,
        activated_deferred_tool_names,
        csl_manager: Some(mgr),
        journal_state: restored_journal.session,
        last_turn_event: restored_journal.last_turn_event,
    }
}

fn prepared_fork_restore_from_restored_journal(
    restored_journal: session_runtime::RestoredJournalState,
) -> PreparedForkRestore {
    PreparedForkRestore {
        history: restored_journal.session.history.clone(),
        recent_tools: restored_journal.session.recent_tools.clone(),
        activated_deferred_tool_names: Vec::new(),
        csl_manager: None,
        journal_state: restored_journal.session,
        last_turn_event: restored_journal.last_turn_event,
    }
}

async fn prepared_fork_restore_from_journal(
    session_id: &str,
) -> Result<PreparedForkRestore, String> {
    let restored_journal = session_runtime::restored_journal_state(session_id)?;
    // Try CSL first — full-fidelity message history via CslManager.
    let base_dir = session_journal::local_owner_sessions_dir();
    let store = std::sync::Arc::new(
        astra_turn_core::conversation_log::file_store::FileCslStore::new(base_dir),
    );
    let mut mgr = astra_turn_core::conversation_log::manager::CslManager::new(
        store,
        session_id.to_string(),
        Default::default(),
    )
    .map_err(|e| format!("initialize CSL state for session {session_id}: {e}"))?;
    match mgr.load().await {
        Ok(Some(mat)) => {
            return Ok(materialize_prepared_fork_restore(
                mgr,
                Some(mat),
                restored_journal,
            ));
        }
        Ok(None) => {}
        Err(e) => {
            return Err(format!("load CSL state for session {session_id}: {e}"));
        }
    }

    // Fall back to prompt-facing transcript before journal summary. The
    // transcript records visible user/assistant/tool stages even when a long
    // turn did not reach a final journal Turn event, so it is the better resume
    // source for interrupted or abandoned work.
    let mut prepared = prepared_fork_restore_from_restored_journal(restored_journal);
    let canonical_history =
        session_continuation::load_session_messages_for_continuation(session_id)
            .map(|messages| session_continuation::history_pairs_from_messages(&messages))
            .unwrap_or_default();
    if canonical_history.len() > prepared.history.len() || prepared.history.is_empty() {
        prepared.history = canonical_history;
    }
    Ok(prepared)
}

async fn load_prepared_fork_restore(
    parent_id: &str,
    new_sid: &str,
    forked_at_turn: u32,
) -> Result<PreparedForkRestore, String> {
    let restored_journal = session_runtime::restored_journal_state(new_sid)?;
    if !restored_journal.exists {
        return Err(format!(
            "missing session journal for forked child {new_sid}"
        ));
    }
    astra_services::session_fork::verify_local_fork_basis(parent_id, new_sid, forked_at_turn)
        .map_err(|error| format!("verify forked child basis before activation: {error}"))?;
    let base_dir = session_journal::local_owner_sessions_dir();
    let store = std::sync::Arc::new(
        astra_turn_core::conversation_log::file_store::FileCslStore::new(base_dir),
    );
    match astra_turn_core::conversation_log::manager::CslManager::new(
        store,
        parent_id.to_string(),
        Default::default(),
    ) {
        Ok(parent_mgr) => match parent_mgr.fork(new_sid, forked_at_turn).await {
            Ok((child_mgr, child_mat)) => Ok(materialize_prepared_fork_restore(
                child_mgr,
                child_mat,
                restored_journal,
            )),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CSL fork failed, child will use journal fallback"
                );
                Ok(prepared_fork_restore_from_restored_journal(
                    restored_journal,
                ))
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                "CSL manager creation failed, child will use journal fallback"
            );
            Ok(prepared_fork_restore_from_restored_journal(
                restored_journal,
            ))
        }
    }
}

async fn apply_prepared_fork_restore(
    state: &mut SessionState,
    parent_id: &str,
    new_sid: &str,
    restored_child: PreparedForkRestore,
    cloud_task_board_copy: Option<CloudTaskBoardCopy>,
) -> Result<ForkTaskBoardRestore, String> {
    let task_restore_plan = if state.task_notify_tx.is_none() {
        let store = state.task_manager.store();
        let child_snapshot = store.load_snapshot_state(new_sid).await.map_err(|error| {
            format!("load existing task board for forked child {new_sid}: {error}")
        })?;
        if child_snapshot.tasks.is_empty() {
            let parent_snapshot = store
                .load_snapshot_state(parent_id)
                .await
                .map_err(|error| {
                    format!("load parent task board for forked child {new_sid}: {error}")
                })?;
            let mut snapshot = astra_tools::task_mgmt::TaskManagerSnapshot {
                tasks: parent_snapshot.tasks,
                next_task_id: parent_snapshot.next_task_id,
                version: child_snapshot.version,
                restore_version: Some(child_snapshot.version),
            };
            snapshot = astra_tools::task_mgmt::prepare_task_snapshot_for_fork(snapshot);
            Some(snapshot)
        } else {
            None
        }
    } else {
        None
    };
    let preserved_existing_child = state.task_notify_tx.is_none() && task_restore_plan.is_none();
    let cloud_task_board_restore = if state.task_notify_tx.is_some() {
        let copy = cloud_task_board_copy.ok_or_else(|| {
            "cloud task board fork copy is unavailable: missing cloud endpoint configuration"
                .to_string()
        })?;
        let status = crate::cli::session::session_todo_client::copy_todos_for_fork(
            &copy.cloud_base,
            copy.token.as_deref(),
            parent_id,
            new_sid,
        )
        .await?;
        match status {
            crate::cli::session::session_todo_client::ForkTaskBoardCopyStatus::Copied => {
                Some(ForkTaskBoardRestore::Copied)
            }
            crate::cli::session::session_todo_client::ForkTaskBoardCopyStatus::PreservedExistingChild => {
                Some(ForkTaskBoardRestore::PreservedExistingChild)
            }
        }
    } else {
        None
    };

    state.set_session_id(new_sid.to_string());
    session_startup::initialize_journal_pub(state, new_sid);
    state.turn = restored_child.journal_state.turn;
    state.total_prompt_tokens = restored_child.journal_state.total_prompt_tokens;
    state.total_completion_tokens = restored_child.journal_state.total_completion_tokens;
    state.total_cache_read_tokens = restored_child.journal_state.total_cache_read_tokens;
    state.total_cache_creation_tokens = restored_child.journal_state.total_cache_creation_tokens;
    state.last_turn_event = restored_child.last_turn_event;
    state.run_id = None;
    state.history = restored_child.history;
    state.recent_tools = restored_child.recent_tools;
    state.activated_deferred_tool_names = restored_child.activated_deferred_tool_names;
    state.csl_manager = restored_child.csl_manager;
    state.last_response = state.history.last().map(|(_, resp)| resp.clone());
    state.continuation_anchor = None;
    state.turns_since_task_use = 0;
    state.turns_since_task_reminder = 0;
    let task_board_restore = if let Some(task_snapshot) = task_restore_plan {
        state.task_manager.restore_snapshot(&task_snapshot).await?;
        ForkTaskBoardRestore::Copied
    } else if let Some(task_board_restore) = cloud_task_board_restore {
        task_board_restore
    } else if preserved_existing_child {
        ForkTaskBoardRestore::PreservedExistingChild
    } else {
        ForkTaskBoardRestore::Copied
    };
    Ok(task_board_restore)
}

/// Result of one transactional local-session fork. The caller owns
/// presentation; this operation owns durable history, task-board, and runtime
/// state restoration so TUI and line-mode commands cannot drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForkSessionOutcome {
    pub new_session_id: String,
    pub events_copied: usize,
    pub preserved_existing_child_task_board: bool,
}

/// Fork `parent_id` and atomically move the active runtime state to the child.
/// Any error before `commit` drops the guard and restores the original state.
pub(crate) async fn fork_session_into_state(
    parent_id: &str,
    label: Option<String>,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
) -> Result<ForkSessionOutcome, String> {
    let fork = fork_local_session(ForkSessionOptions {
        parent_session_id: parent_id.to_string(),
        new_session_id: None,
        label,
        forked_after_turn: None,
        data_branch: None,
        snapshot_spec: None,
    })?;
    let new_session_id = fork.new_session_id;
    let restored_child =
        load_prepared_fork_restore(parent_id, &new_session_id, fork.forked_at_turn).await?;

    let mut fork_guard = ForkStateGuard::new(state);
    let cloud_task_board_copy =
        fork_guard
            .state()
            .task_notify_tx
            .as_ref()
            .map(|_| CloudTaskBoardCopy {
                cloud_base: api.api_origin(),
                token: session_runtime::current_access_token(profile),
            });
    let task_board_restore = apply_prepared_fork_restore(
        fork_guard.state(),
        parent_id,
        &new_session_id,
        restored_child,
        cloud_task_board_copy,
    )
    .await?;
    if let Err(error) =
        crate::cli::session::session_recovery::sync_recovery_snapshot_after_history_edit(
            fork_guard.state(),
        )
        .await
    {
        if error.rollback_failed {
            fork_guard.state().session_persistence_error = Some(error.message.clone());
        }
        return Err(error.message);
    }
    persist_profile_last_session_or_warn(
        profile,
        &new_session_id,
        "slash_session:fork_new_session_id",
    );

    let outcome = ForkSessionOutcome {
        new_session_id,
        events_copied: fork.events_copied,
        preserved_existing_child_task_board: task_board_restore
            == ForkTaskBoardRestore::PreservedExistingChild,
    };
    fork_guard.commit();
    Ok(outcome)
}

async fn restore_journal_history_if_available(
    state: &mut SessionState,
    session_id: &str,
) -> Result<(), String> {
    let restored = prepared_fork_restore_from_journal(session_id).await?;
    if restored.history.len() > state.history.len() || state.history.is_empty() {
        state.history = restored.history;
    }
    if !restored.recent_tools.is_empty() {
        state.recent_tools = restored.recent_tools;
    }
    state.activated_deferred_tool_names = restored.activated_deferred_tool_names;
    state.csl_manager = restored.csl_manager;
    state.last_response = state.history.last().map(|(_, resp)| resp.clone());
    Ok(())
}

async fn apply_restored_session(
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
    restored: RestoredSession,
) -> Result<(), String> {
    let local_journal = session_runtime::restored_journal_state(&restored.session_id)?;
    if !restored.restored_from_cloud && !local_journal.exists {
        return Err(format!(
            "Session {} not found or not owned by user",
            restored.session_id
        ));
    }
    let restored_permission_mode = parse_restored_permission_mode(&restored)?;

    let local_state = local_journal.session;
    let total_cache_read_tokens = restored
        .total_cache_read_tokens
        .max(local_state.total_cache_read_tokens);
    let total_cache_creation_tokens = restored
        .total_cache_creation_tokens
        .max(local_state.total_cache_creation_tokens);
    let prepared_workspace = load_prepared_workspace_restore(&restored)?;
    let prepared_history = prepared_fork_restore_from_journal(&restored.session_id).await?;
    let last_turn_event = local_journal.last_turn_event;
    let user_id = state
        .ingestion_user_id
        .as_deref()
        .filter(|user_id| !user_id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(crate::cli::cli_config::cli_utils::cli_user_id);

    let mut step_restore_error = None;
    // Try new crash recovery state machine first; fall back to legacy restore
    let step_restored =
        match astra_pipeline::crash_recovery::recover_from_crash(&user_id, &restored.session_id) {
            Ok(Some(astra_pipeline::crash_recovery::RecoveryOutcome::AutoRecovered {
                restored: cr_restored,
                ..
            })) => {
                tracing::info!("crash recovery: auto-recovered via state machine");
                Some(cr_restored)
            }
            Ok(Some(astra_pipeline::crash_recovery::RecoveryOutcome::RequiresUserInput {
                pending_decisions,
                restored: cr_restored,
                mut manager,
                ..
            })) => {
                tracing::info!(
                    pending = ?pending_decisions,
                    "crash recovery: requires user input, presenting options"
                );

                // Present each pending decision to the user
                eprintln!();
                eprintln!("  {}", "Crash Recovery Requires User Input".bold().yellow());
                eprintln!("  {}", "The following tool calls need your decision:".dim());

                for (i, (tool_name, decision)) in pending_decisions.iter().enumerate() {
                    eprintln!();
                    eprintln!(
                        "    {} {}",
                        format!("[{}]", i + 1).bold(),
                        tool_name.clone().bold()
                    );

                    let reason = match decision {
                        astra_pipeline::crash_recovery::ToolReplayDecision::RequireUserInput {
                            reason,
                        } => reason.clone(),
                        astra_pipeline::crash_recovery::ToolReplayDecision::InFlightAtCrash {
                            tool_name: tn,
                        } => {
                            format!("Tool '{}' was in-flight when crash occurred", tn)
                        }
                        _ => "Unknown".to_string(),
                    };
                    eprintln!("      Reason: {}", reason.dim());
                }

                eprintln!();
                eprintln!("  {}", "Options:".bold());
                eprintln!("    [r] Replay - Re-execute the tool");
                eprintln!("    [s] Skip - Skip this tool (use cached result if available)");
                eprintln!("    [a] Abort - Abort recovery and fail");
                eprintln!();
                eprint!(
                    "  {} ",
                    "Choose action for all pending tools (r/s/a):".bold()
                );
                let _ = std::io::stderr().flush();

                let mut input = String::new();
                if std::io::stdin().read_line(&mut input).is_err() {
                    return Err("Failed to read user input for crash recovery".to_string());
                }

                let choice = input.trim().to_lowercase();
                match choice.as_str() {
                    "r" | "replay" | "s" | "skip" => {
                        let label = if choice.starts_with('r') {
                            "replay"
                        } else {
                            "skip"
                        };
                        eprintln!(
                            "  {} User chose to {} all pending tools",
                            "✓".green(),
                            label
                        );
                        // force_complete transitions Replaying -> Recovered, bypassing pending checks
                        if let Err(e) = manager.force_complete() {
                            return Err(format!("Failed to apply {} decisions: {}", label, e));
                        }
                        Some(cr_restored)
                    }
                    "a" | "abort" => {
                        eprintln!("  {} User chose to abort recovery", "✗".red());
                        return Err("User aborted crash recovery".to_string());
                    }
                    _ => {
                        eprintln!("  {} Invalid choice, aborting recovery", "✗".red());
                        return Err(format!("Invalid user choice: {}", choice));
                    }
                }
            }
            Ok(None) => {
                // No crash detected — use legacy restore
                match astra_pipeline::step_restore::restore_session(&user_id, &restored.session_id)
                {
                    Ok(r) => r,
                    Err(astra_pipeline::step_restore::RestoreError::IoError(error)) => {
                        return Err(format!(
                            "Failed to read local step checkpoint for user_id={} session_id={}: {}",
                            user_id, restored.session_id, error
                        ));
                    }
                    Err(error) => {
                        step_restore_error = Some(error.to_string());
                        None
                    }
                }
            }
            Err(cr_err) => {
                tracing::warn!(
                    error = %cr_err,
                    "crash recovery state machine failed, falling back to legacy restore"
                );
                match astra_pipeline::step_restore::restore_session(&user_id, &restored.session_id)
                {
                    Ok(r) => r,
                    Err(astra_pipeline::step_restore::RestoreError::IoError(error)) => {
                        return Err(format!(
                            "Failed to read local step checkpoint for user_id={} session_id={}: {}",
                            user_id, restored.session_id, error
                        ));
                    }
                    Err(error) => {
                        step_restore_error = Some(error.to_string());
                        None
                    }
                }
            }
        };
    let has_cloud_heavy_fallback = !restored.conversation_messages.is_empty()
        || !restored.blocked_tools.is_empty()
        || restored.approval_overrides.is_some()
        || restored.interruption.is_some()
        || restored.compaction_state.is_some();
    if step_restore_error.is_some()
        && !has_cloud_heavy_fallback
        && let Some(error) = step_restore_error.as_ref()
    {
        return Err(format!(
            "Failed to restore local step checkpoint for user_id={} session_id={}: {}",
            user_id, restored.session_id, error
        ));
    }
    if let Some(error) = step_restore_error.as_ref() {
        tracing::warn!(
            error = %error,
            "local step checkpoint restore failed; continuing with fallback state"
        );
    }
    let session_memory = super::slash_memory::load_current_session_memory_body_with_profile(
        api,
        profile,
        &restored.session_id,
    )
    .await;
    persist_resumed_workspace_metadata(
        &restored,
        total_cache_read_tokens,
        total_cache_creation_tokens,
        prepared_workspace.workspace.as_ref(),
    )?;

    state.prepare_for_session_rebind().await;
    state.reset_for_session_restore();
    state.set_session_id(restored.session_id.clone());
    state.turn = restored.turn_count;
    state.total_prompt_tokens = restored.total_tokens_in;
    state.total_completion_tokens = restored.total_tokens_out;
    state.total_cache_read_tokens = total_cache_read_tokens;
    state.total_cache_creation_tokens = total_cache_creation_tokens;
    state.recent_tools = restored.recent_tools.clone();
    if let Some(mode) = restored_permission_mode {
        state.perm_manager.set_mode(mode);
    }
    apply_prepared_workspace_restore(state, &prepared_workspace);

    if let Some(step_restored) = step_restored {
        let summary = astra_pipeline::step_restore::restore_summary(&step_restored);
        for tool in &step_restored.blocked_tools {
            if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
                state
                    .tool_health_entries
                    .push(blocked_tool_health_entry(tool.clone()));
            }
        }
        if state.recent_tools.is_empty() {
            state.recent_tools = step_restored.recent_tools;
        }
        apply_resume_recovery_state(
            state,
            step_restored.interruption.as_ref(),
            step_restored.compaction_state.as_ref(),
        );
        apply_runtime_recovery_state(
            state,
            step_restored.pipeline_state.as_ref(),
            step_restored.compaction_state.as_ref(),
            step_restored.consecutive_context_window_errors,
        );
        if let Some(ref ao_json) = step_restored.approval_overrides {
            state.perm_manager.merge_restored_overrides(ao_json);
        }
        eprintln!("  {} {}", "↻".magenta(), summary.dim());
    } else if has_cloud_heavy_fallback {
        apply_restored_cloud_heavy_state(state, &restored);
        apply_resume_recovery_state(
            state,
            restored.interruption.as_ref(),
            restored.compaction_state.as_ref(),
        );
        apply_runtime_recovery_state(
            state,
            restored.pipeline_state.as_ref(),
            restored.compaction_state.as_ref(),
            0,
        );
        eprintln!("  {} Restored step checkpoint from cloud", "☁".magenta());
    }

    match normalize_model_override(restored.model.as_deref()) {
        Some(m) => {
            state.model = Some(m.to_string());
            let base = astra_turn_core::thinking_config::resolve_model_thinking(m).0;
            state.cached_pricing = slash_stats::fallback_pricing(base);
            state.context_budget =
                prompts::ContextBudget::from_runtime_config(&state.runtime_config, Some(base));
        }
        None => {
            state.model = None;
            state.context_budget =
                prompts::ContextBudget::from_runtime_config(&state.runtime_config, None);
        }
    }
    crate::cli::slash::slash_config::set_active_model_for_display(state.model.clone());
    crate::cli::slash::slash_config::set_active_model_id_for_request(None);

    if prepared_history.history.len() > state.history.len() || state.history.is_empty() {
        state.history = prepared_history.history;
    }
    if !prepared_history.recent_tools.is_empty() {
        state.recent_tools = prepared_history.recent_tools;
    }
    state.activated_deferred_tool_names = prepared_history.activated_deferred_tool_names;
    state.csl_manager = prepared_history.csl_manager;
    state.last_response = state.history.last().map(|(_, resp)| resp.clone());
    state.last_turn_event = last_turn_event;
    let fallback_resume_messages;
    let canonical_resume_messages = if !restored.conversation_messages.is_empty() {
        restored.conversation_messages.as_slice()
    } else {
        fallback_resume_messages =
            session_continuation::load_session_messages_for_continuation(&restored.session_id)
                .unwrap_or_else(|| session_projection::history_as_messages(&state.history));
        fallback_resume_messages.as_slice()
    };
    session_projection::seed_continuation_objective_from_messages(state, canonical_resume_messages);
    session_projection::rebuild_continuation_anchor_from_live_state(state).await;
    state.continuation_anchor = session_projection::merge_continuation_anchor_with_session_memory(
        state.continuation_anchor.take(),
        session_memory.as_deref(),
    );
    let session_resume_hydration =
        match astra_turn_core::resume_hydration::build_resume_hydration_hint_from_messages(
            canonical_resume_messages,
        ) {
            Ok(Some(hint)) => hint,
            Ok(None) => astra_turn_core::resume_hydration::build_resume_hydration_failure_hint(
                "resume restored session metadata but no prompt-facing transcript/history",
            ),
            Err(error) => {
                tracing::warn!(
                    session_id = %restored.session_id,
                    error = %error,
                    "resume hydration degraded: restored typed turn metadata is invalid"
                );
                astra_turn_core::resume_hydration::build_resume_hydration_failure_hint(
                    "restored session contains invalid typed turn metadata",
                )
            }
        };
    state.resume_guidance = astra_turn_core::resume_hydration::merge_resume_hints(
        Some(session_resume_hydration),
        state.resume_guidance.take(),
    );

    if let Some(ref json) = restored.executing_plan_json {
        state.executing_plan = serde_json::from_str(json).ok();
    }
    if let Some(ref goal) = restored.plan_goal {
        state.executing_plan_goal = Some(goal.clone());
        session_startup::steer_observability_goal(state, goal);
    }
    if let Some(ref json) = restored.plan_config_json {
        state.plan_execution_config = serde_json::from_str(json).ok();
    }
    state.plan_execution_rounds = restored.plan_execution_rounds;
    state.plan_execution_corrections = restored.plan_corrections.clone();

    if let Some(ref json) = restored.contract_json
        && let Ok(contract) = serde_json::from_str::<astra_services::TaskContract>(json)
    {
        let work_dir = std::env::current_dir().unwrap_or_default();
        // Verification judge runs server-side via server_proxy_judge; the server resolves
        // the reasoning Offering via reasoning_offering_id → governed default
        // fallback. No local cloud judge.
        let cloud_judge: Option<std::sync::Arc<dyn astra_services::LlmJudge>> = None;
        let server_proxy_judge: Option<std::sync::Arc<dyn astra_services::LlmJudge>> =
            match get_profile_and_token(profile) {
                Ok((_, _, _, token)) => Some(std::sync::Arc::new(
                    durable_bridge::ServerProxyLlmJudge::new(api.clone(), token),
                )),
                Err(_) => None,
            };

        let session_dir =
            astra_services::session_workspace::workspace_dir_for(&restored.session_id);
        let lifecycle = durable_bridge::create_local_lifecycle_full(
            &session_dir,
            &work_dir,
            None,
            Some(&restored.session_id),
            state.ingestion_user_id.as_deref(),
            cloud_judge,
            server_proxy_judge,
        );
        state.durable_task_state = Some(durable_bridge::DurableTaskState {
            contract,
            lifecycle,
            last_report: None,
        });
    }

    session_startup::initialize_journal_pub(state, &restored.session_id);
    persist_profile_last_session_or_warn(
        profile,
        &restored.session_id,
        "slash_session:restore_session_into_state",
    );

    let source = session_source_surface(&restored.last_status, restored.restored_from_cloud, false);
    eprintln!(
        "  {} Resumed session {} ({}, {} turns, {} checkpoints)",
        theme::icon_ok(),
        restored.session_id[..8.min(restored.session_id.len())].magenta(),
        source.label(),
        restored.turn_count,
        restored.checkpoint_count,
    );
    if let Some(warning) = resume_persistence_warning(state.session_persistence_error.as_deref()) {
        eprintln!("  {}", warning.yellow());
    }
    if let Some(ref trace) = restored.last_context_trace {
        let preview = trace.preview();
        if !preview.is_empty() {
            eprintln!("    {} {}", "Last trace:".dim(), preview.dim());
        }
    }

    if let Some(ref plan) = state.executing_plan {
        let done = plan.items_done();
        let total = plan.subtasks.len();
        let pct = plan.progress_pct();
        eprintln!(
            "  {} Paused plan restored: {}/{} subtasks done ({}%)",
            "📋".magenta(),
            done,
            total,
            pct,
        );
        if let Some(ref goal) = state.executing_plan_goal {
            eprintln!("    {} {}", "Goal:".dim(), goal.as_str().dim());
        }
        eprintln!(
            "    {}",
            "Paused plan restored. Inspect or edit it with slash commands; use correct … / rewind N to adjust; any other line abandons it."
                .dim()
        );
    }

    Ok(())
}

pub(crate) async fn restore_session_into_state(
    session_id: &str,
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
) -> Result<(), String> {
    let restored =
        session_restore_client::restore_session_snapshot_with_client(profile, api, session_id)
            .await?;
    let Some(restored) = restored else {
        if matches!(
            preflight_remote_resume_session(api, profile, session_id).await,
            SessionResumePreflight::Missing
        ) {
            clear_profile_last_session_if_matches_or_warn(
                profile,
                session_id,
                "slash_session:restore_session_snapshot",
            );
            return Err(format!(
                "Session {session_id} no longer exists on the server and has no local resumable state."
            ));
        }
        return Err(format!(
            "Session {session_id} has no resumable workspace/checkpoint state. Use /resume to inspect available sessions."
        ));
    };
    apply_restored_session(profile, api, state, restored).await
}

// ═══════════════════════════════════════════════════════════ Resume ═══════

pub(crate) async fn handle_resume_command(
    arg: &str,
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
) {
    // If no session_id given, list and let user pick
    let effective_arg;
    if arg.is_empty() {
        let candidates = match load_resumable_session_candidates(profile, api, 20).await {
            Ok(candidates) => candidates,
            Err(error) => {
                eprintln!("  {} {}", theme::icon_err(), error.red());
                return;
            }
        };

        if let Some(error) = candidates.cloud_scan_error.as_ref() {
            eprintln!("  {}", error.as_str().yellow());
        }
        if let Some(error) = candidates.local_scan_error.as_ref() {
            eprintln!("  {}", error.as_str().yellow());
        }

        let result = candidates.sessions;

        if result.is_empty() {
            eprintln!("{}", "  No resumable sessions found.".dim());
            return;
        }

        let sessions = &result[..result.len().min(10)];

        // Enrich with local metadata (workspace + journal peek) — fast, no DB
        struct SessionDisplay {
            idx: usize,
            session_id: String,
            title: Option<String>,
            first_prompt: Option<String>,
            turn_count: u32,
            model: Option<String>,
            cwd_short: Option<String>,
            git_branch: Option<String>,
            source: String,
            workspace_error: bool,
            persistence_error: Option<String>,
            has_plan: bool,
            age: String,
        }

        let mut items: Vec<SessionDisplay> = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            let peek = session_journal::peek_session_meta(&s.session_id);
            let (ws, workspace_error) =
                match astra_services::session_workspace::read_workspace_optional(&s.session_id) {
                    Ok(workspace) => (workspace, false),
                    Err(error) => {
                        tracing::warn!(
                            "resume picker failed to read workspace for {}: {}",
                            s.session_id,
                            error
                        );
                        (None, true)
                    }
                };

            // Title: cloud title > workspace summary > first prompt preview
            let title = s
                .title
                .clone()
                .or_else(|| ws.as_ref().and_then(|w| w.summary.clone()))
                .or_else(|| peek.as_ref().and_then(|p| p.first_prompt.clone()));

            let first_prompt = peek.as_ref().and_then(|p| p.first_prompt.clone());

            // Model: cloud > workspace > journal peek
            let model = s
                .model
                .clone()
                .or_else(|| ws.as_ref().and_then(|w| w.model.clone()))
                .or_else(|| peek.as_ref().and_then(|p| p.model.clone()));

            // cwd: shorten to last 2 path components
            let cwd_short = ws.as_ref().map(|w| {
                let parts: Vec<&str> = w.cwd.split('/').filter(|s| !s.is_empty()).collect();
                if parts.len() <= 2 {
                    w.cwd.clone()
                } else {
                    format!("…/{}", parts[parts.len() - 2..].join("/"))
                }
            });

            let git_branch = s
                .git_branch
                .clone()
                .or_else(|| ws.as_ref().and_then(|w| w.git_branch.clone()));
            let persistence_error = ws
                .as_ref()
                .and_then(|w| w.last_persistence_error.clone())
                .filter(|error| !error.trim().is_empty());

            let source =
                session_source_surface(&s.last_status, s.restored_from_cloud, workspace_error)
                    .badge()
                    .to_string();

            let has_plan = ws.as_ref().is_some_and(|w| w.executing_plan_json.is_some());

            // Age: from workspace or journal timestamp
            let age = ws
                .as_ref()
                .map(|w| &w.updated_at)
                .or_else(|| peek.as_ref().and_then(|p| p.created_at.as_ref()))
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| {
                    let dur = chrono::Utc::now().signed_duration_since(dt);
                    if dur.num_minutes() < 60 {
                        format!("{}m ago", dur.num_minutes())
                    } else if dur.num_hours() < 24 {
                        format!("{}h ago", dur.num_hours())
                    } else {
                        format!("{}d ago", dur.num_days())
                    }
                })
                .unwrap_or_default();

            items.push(SessionDisplay {
                idx: i + 1,
                session_id: s.session_id.clone(),
                title,
                first_prompt,
                turn_count: s.turn_count,
                model,
                cwd_short,
                git_branch,
                source,
                workspace_error,
                persistence_error,
                has_plan,
                age,
            });
        }

        eprintln!(
            "\n{}",
            "─── Resumable Sessions ──────────────────────────".bold()
        );
        for s in &items {
            // Line 1: [N]  title or first prompt  (age)
            let display_text = s
                .title
                .as_deref()
                .or(s.first_prompt.as_deref())
                .unwrap_or("(no prompt)");
            let display_truncated: String = display_text.chars().take(60).collect();
            let plan_badge = if s.has_plan { " 📋" } else { "" };
            eprintln!(
                "  {}  {}{}  {}",
                format!("[{}]", s.idx).magenta().bold(),
                display_truncated,
                plan_badge,
                s.age.as_str().dim(),
            );
            // Line 2: context details
            let short_id = &s.session_id[..8.min(s.session_id.len())];
            let model_str = s.model.as_deref().unwrap_or("?");
            let branch_str = s
                .git_branch
                .as_deref()
                .map(|b| format!(" {b}"))
                .unwrap_or_default();
            let cwd_str = s.cwd_short.as_deref().unwrap_or("");
            eprintln!(
                "      {} {} {} turns · {}{} {}",
                s.source.as_str().dim(),
                short_id.dim(),
                s.turn_count,
                model_str.dim(),
                branch_str.dim(),
                cwd_str.dim(),
            );
            if s.workspace_error {
                eprintln!(
                    "      {}",
                    "workspace.yaml invalid; using journal/cloud metadata".yellow()
                );
            }
            if let Some(error) = s.persistence_error.as_deref() {
                if let Some(warning) = resume_persistence_warning(Some(error)) {
                    eprintln!("      {}", warning.yellow());
                }
            }
        }
        eprintln!();
        eprint!("  {} ", "Select (number or Enter to cancel):".bold());
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            if let Ok(n) = input.trim().parse::<usize>() {
                if n >= 1 && n <= sessions.len() {
                    effective_arg = sessions[n - 1].session_id.clone();
                } else {
                    eprintln!("{}", "  Cancelled.".dim());
                    return;
                }
            } else {
                eprintln!("{}", "  Cancelled.".dim());
                return;
            }
        } else {
            return;
        }
    } else {
        effective_arg = arg.to_string();
    }
    let arg = effective_arg.as_str();

    // Resolve prefix via local journal first
    let session_id = match session_journal::resolve_session_id(arg) {
        Ok(resolved) => {
            if resolved != arg {
                eprintln!(
                    "  {} Resolved {} → {}",
                    theme::icon_ok(),
                    arg.magenta(),
                    resolved.as_str().magenta()
                );
            }
            resolved
        }
        Err(_) => arg.to_string(),
    };

    // Show preview if user explicitly typed a session ID (not from picker)
    if !arg.is_empty() {
        // Show session preview
        let (ws, workspace_error) = match session_workspace::read_workspace_optional(&session_id) {
            Ok(workspace) => (workspace, None),
            Err(error) => (None, Some(error)),
        };
        let peek = session_journal::peek_session_meta(&session_id);

        eprintln!(
            "\n{}",
            "─── Session Preview ─────────────────────────────"
                .bold()
                .magenta()
        );

        // Session ID
        let short_id = &session_id[..8.min(session_id.len())];
        eprintln!(
            "  {:<14} {}",
            "session:".dim(),
            format!("{short_id}…").magenta()
        );

        // Summary/Title
        let summary = ws
            .as_ref()
            .and_then(|w| w.summary.clone())
            .or_else(|| peek.as_ref().and_then(|p| p.first_prompt.clone()))
            .map(|s| {
                let truncated: String = s.chars().take(70).collect();
                if s.chars().count() > 70 {
                    format!("{truncated}…")
                } else {
                    truncated
                }
            })
            .unwrap_or_else(|| "(no summary)".to_string());
        eprintln!("  {:<14} {}", "summary:".dim(), summary);

        // Turn count
        if let Some(ref w) = ws {
            eprintln!(
                "  {:<14} {} turns",
                "progress:".dim(),
                w.turn_count.to_string().magenta()
            );
            if let Some(warning) = resume_persistence_warning(w.last_persistence_error.as_deref()) {
                eprintln!("  {:<14} {}", "warning:".dim(), warning.yellow());
            }
        } else if peek.is_some() {
            let turns = session_journal::count_turns(&session_id);
            eprintln!(
                "  {:<14} {} turns",
                "progress:".dim(),
                turns.to_string().magenta()
            );
        }

        // Model
        let model = ws
            .as_ref()
            .and_then(|w| w.model.clone())
            .or_else(|| peek.as_ref().and_then(|p| p.model.clone()))
            .unwrap_or_else(|| "?".to_string());
        eprintln!("  {:<14} {}", "model:".dim(), model.magenta());

        // Cwd + git branch
        if let Some(ref w) = ws {
            eprintln!(
                "  {:<14} {}",
                "directory:".dim(),
                tilde_path(&w.cwd).as_str().magenta()
            );
            if let Some(ref b) = w.git_branch {
                let head = w
                    .git_head
                    .as_deref()
                    .map(|h| &h[..7.min(h.len())])
                    .unwrap_or("");
                eprintln!(
                    "  {:<14} {} @ {}",
                    "branch:".dim(),
                    b.as_str().magenta(),
                    head.dim()
                );
            }
        }

        // Status
        if let Some(ref w) = ws {
            let status = session_workspace_status_surface(w.status.as_str());
            eprintln!(
                "  {:<14} {} {}",
                "status:".dim(),
                status.icon(),
                status.label().magenta()
            );
        }

        // Age
        let age = ws
            .as_ref()
            .map(|w| &w.updated_at)
            .or_else(|| peek.as_ref().and_then(|p| p.created_at.as_ref()))
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| {
                let dur = chrono::Utc::now().signed_duration_since(dt);
                if dur.num_minutes() < 60 {
                    format!("{}m ago", dur.num_minutes())
                } else if dur.num_hours() < 24 {
                    format!("{}h ago", dur.num_hours())
                } else {
                    format!("{}d ago", dur.num_days())
                }
            });
        if let Some(age) = age {
            eprintln!("  {:<14} {}", "last active:".dim(), age.dim());
        }

        if let Some(error) = workspace_error.as_ref() {
            eprintln!(
                "  {:<14} {}",
                "workspace:".dim(),
                format!("invalid ({error})").yellow()
            );
        }

        // Plan status
        if let Some(ref w) = ws {
            if w.plan_goal.is_some() {
                let goal = w.plan_goal.as_deref().unwrap_or("");
                let goal_short: String = goal.chars().take(50).collect();
                eprintln!("  {:<14} 📋 {}", "plan:".dim(), goal_short.yellow());
            }
        }

        eprintln!();

        // Confirm
        eprint!("  {} ", "Resume this session? [Y/n]:".bold());
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            let answer = input.trim().to_lowercase();
            if !answer.is_empty() && answer != "y" && answer != "yes" {
                eprintln!("{}", "  Cancelled.".dim());
                return;
            }
        } else {
            return;
        }
    }

    if let Err(e) = restore_session_into_state(&session_id, profile, api, state).await {
        let hint = resume_restore_hint(&e);
        eprintln!("  {} {}", theme::icon_err(), e.red());
        eprintln!("{}", format!("  {hint}").dim());
    }
}

#[cfg(test)]
mod resume_tests {
    use super::{
        ForkStateGuard, ForkStateSnapshot, ForkTaskBoardRestore, PreparedForkRestore,
        SessionListFilterOptions, SessionListFilterOutcome, apply_heavy_checkpoint_fallback,
        apply_prepared_fork_restore, apply_restored_session, apply_resume_recovery_state,
        build_session_list_entries, build_step_resume_guidance, filter_session_list_entries,
        load_prepared_fork_restore, load_resumable_session_candidates,
        restore_journal_history_if_available, restore_session_into_state,
        resume_persistence_warning, session_restore_client, session_runtime, session_startup,
        switch_session_into_state, workspace_summary_line,
    };
    use crate::cli::permission_manager::PermissionMode;
    use crate::cli::session::session_state::{ContinuationAnchor, SessionState};
    use astra_services::session_journal::{self, JournalDirGuard};
    use astra_services::session_restore::RestoredSession;
    use astra_services::session_workspace;
    use astra_tools::task_mgmt::{SessionTask, TaskMutation, TaskStore};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    struct FailingLoadTaskStore;

    #[async_trait::async_trait]
    impl TaskStore for FailingLoadTaskStore {
        async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
            Err(format!("forced load failure for {session_id}"))
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn mutate(
            &self,
            session_id: &str,
            _mutation: TaskMutation,
        ) -> Result<astra_tools::task_mgmt::TaskMutationOutcome, String> {
            Err(format!("forced mutate failure for {session_id}"))
        }

        async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
            Err(format!("forced next id failure for {session_id}"))
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn write_local_resumable_session(session_id: &str, turn_count: u32) {
        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(session_id),
                turn_count,
                Some("gpt-5"),
                "continue",
                "restored",
                0,
                15,
                7,
                8,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(session_id),
                turn_count,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 1,
                    "turns_completed": turn_count,
                    "remaining_turns": 4,
                }),
            ))
            .unwrap();

        let cwd = std::env::current_dir().unwrap();
        let mut ws = session_workspace::WorkspaceMetadata::with_context(
            session_id,
            "gpt-5",
            &cwd.display().to_string(),
            Some("main"),
        );
        ws.turn_count = turn_count;
        ws.total_tokens_in = 15;
        ws.total_tokens_out = 7;
        ws.total_cache_read_tokens = 9;
        ws.total_cache_creation_tokens = 3;
        ws.status = "active".to_string();
        session_workspace::write_workspace(&ws).unwrap();
    }

    fn write_local_step_checkpoint_with_interruption(session_id: &str, turn_count: u32) {
        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            format!("step-{turn_count}"),
            format!("task-{turn_count}"),
            session_id.to_string(),
            astra_pipeline::step_protocol::ExecutionCursor::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.messages = vec![
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "restoring interrupted lifecycle work"}),
        ];
        heavy.recent_tools = vec!["bash".into(), "introspect".into()];
        heavy.interruption = Some(serde_json::json!({
            "kind": "rate_limited",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 1,
            "turns_completed": turn_count,
            "remaining_turns": 2,
        }));
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            session_id,
            turn_count,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
    }

    fn write_local_step_checkpoint_with_approval_overrides(
        session_id: &str,
        turn_count: u32,
        approval_overrides: serde_json::Value,
    ) {
        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            format!("step-{turn_count}"),
            format!("task-{turn_count}"),
            session_id.to_string(),
            astra_pipeline::step_protocol::ExecutionCursor::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.messages = vec![
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "restoring session approvals"}),
        ];
        heavy.approval_overrides = Some(approval_overrides);
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            session_id,
            turn_count,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
    }

    fn write_workspace_lifecycle_state(session_id: &str) {
        let mut ws = astra_services::session_workspace::read_workspace(session_id).unwrap();
        let executing_plan = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "plan-1".into(),
                    title: "Capture restore summary".into(),
                    status: astra_services::task_orchestrator::TaskStatus::Completed,
                    ..Default::default()
                },
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "plan-2".into(),
                    title: "Verify lifecycle state".into(),
                    status: astra_services::task_orchestrator::TaskStatus::InProgress,
                    ..Default::default()
                },
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "plan-3".into(),
                    title: "Close recovery loop".into(),
                    status: astra_services::task_orchestrator::TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let contract = astra_services::durable_task::TaskContract {
            contract_id: "contract-restore".into(),
            task_id: "task-restore".into(),
            goal: "Ship lifecycle UX".into(),
            scope: astra_services::durable_task::TaskScope::default(),
            subtasks: vec![
                astra_services::durable_task::DurableSubtask {
                    id: "verify-1".into(),
                    title: "Capture restore summary".into(),
                    stage: astra_services::durable_task::SubtaskStage::Completed,
                    ..Default::default()
                },
                astra_services::durable_task::DurableSubtask {
                    id: "verify-2".into(),
                    title: "Verify restore contract".into(),
                    stage: astra_services::durable_task::SubtaskStage::AwaitingVerification,
                    ..Default::default()
                },
            ],
            global_verification: Vec::new(),
            version: 1,
            status: astra_services::durable_task::ContractStatus::Active,
            created_at: "now".into(),
            updated_at: "now".into(),
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
        };
        ws.executing_plan_json = Some(serde_json::to_string(&executing_plan).unwrap());
        ws.plan_goal = Some("Ship lifecycle UX".into());
        ws.plan_execution_rounds = 4;
        ws.plan_corrections = vec!["tighten resume messaging".into()];
        ws.contract_json = Some(serde_json::to_string(&contract).unwrap());
        astra_services::session_workspace::write_workspace(&ws).unwrap();
    }

    fn write_profile_with_token(session_id: &str) {
        let mut creds = crate::cli::cli_config::cli_utils::CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            crate::cli::cli_config::cli_utils::Profile {
                access_token: Some("test-token".into()),
                last_session_id: Some(session_id.to_string()),
                ..Default::default()
            },
        );
        crate::cli::cli_config::cli_utils::save_credentials(&creds).unwrap();
    }

    fn write_completed_read_step_event(session_id: &str, turn_count: u32, created_at: u64) {
        let args = serde_json::json!({"path": "src/lib.rs"});
        let idem_key = astra_pipeline::step_protocol::IdempotencyKey::semantic("read_file", &args);
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let mut event_store =
            astra_pipeline::step_checkpoint::FileBackedEventStore::empty(&user_id, session_id);
        let _ = <astra_pipeline::step_checkpoint::FileBackedEventStore as astra_pipeline::step_protocol::StepEventStore>::append(
            &mut event_store,
            astra_pipeline::step_protocol::StepEvent {
                event_id: format!("completed-read-{turn_count}"),
                canonical_event_id: None,
                step_id: format!("step-{turn_count}"),
                event_type: astra_pipeline::step_protocol::StepEventType::ToolCallCompleted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({
                    "tool_name": "read_file",
                    "idempotency_key": idem_key.cache_key(),
                    "output": "cached src/lib.rs",
                    "is_error": false,
                })),
                created_at,
            },
        );
    }

    fn write_local_step_checkpoint_with_compaction_state(session_id: &str, turn_count: u32) {
        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            format!("step-{turn_count}"),
            format!("task-{turn_count}"),
            session_id.to_string(),
            astra_pipeline::step_protocol::ExecutionCursor::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.messages = vec![
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "context was compacted"}),
        ];
        heavy.interruption = Some(serde_json::json!({
            "kind": "context_overflow",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 2,
            "turns_completed": turn_count,
            "remaining_turns": 1,
        }));
        heavy.compaction_state = Some(serde_json::json!({
            "attempt_count": 3,
            "cumulative_tokens_freed": 15000,
            "last_tokens_freed": 4000,
            "last_was_insufficient": true,
        }));
        heavy.pipeline_state = Some(serde_json::json!({
            "stats": {"cache_hit_ratio_ema": 0.42},
            "recovery": {"ptl_error_count": 2},
        }));
        heavy.consecutive_context_window_errors = 2;
        let completed_event_created_at = heavy.light.created_at.saturating_add(1);
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            session_id,
            turn_count,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
        write_completed_read_step_event(session_id, turn_count, completed_event_created_at);
    }

    fn write_invalid_local_step_checkpoint(session_id: &str, turn_count: u32) {
        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            format!("step-{turn_count}"),
            format!("task-{turn_count}"),
            session_id.to_string(),
            astra_pipeline::step_protocol::ExecutionCursor::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.light.protocol_version = 0;
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            session_id,
            turn_count,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
    }

    #[test]
    fn apply_heavy_checkpoint_fallback_restores_history_and_approval_overrides() {
        use astra_turn_core::approval_fingerprint::{ApprovalFingerprint, FingerprintedOverrides};

        let mut state = SessionState::default();
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(
            ApprovalFingerprint::shell("bash", "git commit -m 'wip'", false),
            true,
        );
        let approval_json = overrides.to_json().expect("non-empty overrides");

        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            "step-1".into(),
            "task-1".into(),
            "agent-1".into(),
            Default::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.messages = vec![
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        heavy.recent_tools = vec!["rg".into()];
        heavy.blocked_tools = vec!["bash".into()];
        heavy.approval_overrides = Some(approval_json.clone());

        apply_heavy_checkpoint_fallback(&mut state, &heavy);

        assert_eq!(
            state.history,
            vec![("continue".to_string(), "done".to_string())]
        );
        assert_eq!(state.recent_tools, vec!["rg".to_string()]);
        assert!(
            state
                .tool_health_entries
                .iter()
                .any(|entry| entry.name == "bash")
        );
        let exported = state
            .perm_manager
            .export_session_overrides()
            .expect("restored approval overrides");
        assert_eq!(serde_json::to_value(exported).unwrap(), approval_json);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_journal_history_if_available_preserves_existing_history_when_journal_missing()
    {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let mut state = SessionState::default();
        state.history = vec![("from-cloud".to_string(), "still-here".to_string())];

        restore_journal_history_if_available(&mut state, "missing-session")
            .await
            .unwrap();

        assert_eq!(
            state.history,
            vec![("from-cloud".to_string(), "still-here".to_string())]
        );
    }

    #[test]
    fn apply_resume_recovery_state_ignores_stall_derived_resume_restricted_tools() {
        let mut state = SessionState::default();
        apply_resume_recovery_state(
            &mut state,
            Some(&serde_json::json!({
                "kind": "budget_exhausted",
                "resumable": true,
                "stall_signal": "redundant_reads=4",
                "resume_restricted_tools": ["view", "read_file", "view"]
            })),
            None,
        );

        assert!(
            state.resume_restricted_tools.is_empty(),
            "stall-derived resume restrictions are soft guidance, not hard tool blocks"
        );
    }

    #[test]
    fn build_step_resume_guidance_decodes_compaction_state_schema() {
        let guidance = build_step_resume_guidance(
            Some(&serde_json::json!({
                "kind": "context_overflow",
                "resumable": true,
                "has_checkpoint": true,
                "tool_calls_completed": 1,
                "turns_completed": 2,
                "remaining_turns": 1,
            })),
            Some(&serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_was_insufficient": true,
            })),
        )
        .expect("guidance");

        assert!(guidance.contains("3 attempt(s)"), "{guidance}");
        assert!(guidance.contains("15000 tokens freed"), "{guidance}");
        assert!(guidance.contains("insufficient"), "{guidance}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_uses_checkpoint_compaction_state_for_resume_guidance() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-compact-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);
        write_local_step_checkpoint_with_compaction_state(&session_id, 2);

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            ..Default::default()
        };
        let mut state = SessionState::default();
        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("apply restored session");

        let guidance = state.resume_guidance.expect("resume guidance");
        assert!(guidance.contains("3 attempt(s)"), "{guidance}");
        assert!(guidance.contains("15000 tokens freed"), "{guidance}");
        assert!(guidance.contains("insufficient"), "{guidance}");
        assert_eq!(
            state.runtime_compaction_state,
            Some(serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_tokens_freed": 4000,
                "last_was_insufficient": true,
            }))
        );
        assert_eq!(
            state.runtime_pipeline_state,
            Some(serde_json::json!({
                "stats": {"cache_hit_ratio_ema": 0.42},
                "recovery": {"ptl_error_count": 2},
            }))
        );
        assert_eq!(state.runtime_consecutive_context_window_errors, 2);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_rebinds_root_mailbox_and_replaces_live_session_overrides() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-overrides-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);

        let mut restored_overrides =
            astra_turn_core::approval_fingerprint::FingerprintedOverrides::default();
        restored_overrides.insert(
            astra_turn_core::approval_fingerprint::ApprovalFingerprint::bare("read_file"),
            true,
        );
        write_local_step_checkpoint_with_approval_overrides(
            &session_id,
            2,
            restored_overrides
                .to_json()
                .expect("restored overrides should serialize"),
        );

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            ..Default::default()
        };
        let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = std::sync::Arc::new(
            astra_runtime::server::delegation::engine::DelegationTracker::new(),
        );
        let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
            transport.clone(),
            tracker,
        ));
        let live_root_addr = astra_messaging::AgentAddress::new("live-session", "main");

        let mut state = SessionState::default();
        state.set_session_id("live-session");
        state.root_mailbox = Some(router.register(live_root_addr.clone(), None).await.unwrap());
        state.perm_manager.record_approval("bash", None, false);

        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("apply restored session");

        let restored_session_overrides = state
            .perm_manager
            .export_session_overrides()
            .expect("checkpoint overrides should be restored");
        let old_bash = astra_turn_core::approval_fingerprint::ApprovalFingerprint::bare("bash");
        let restored_read_file =
            astra_turn_core::approval_fingerprint::ApprovalFingerprint::bare("read_file");
        assert_eq!(restored_session_overrides.check(&old_bash), None);
        assert_eq!(
            restored_session_overrides.check(&restored_read_file),
            Some(true)
        );
        assert!(state.root_mailbox.is_none());
        router
            .register(live_root_addr, None)
            .await
            .expect("resume should unregister the prior root mailbox");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_rebuilds_corrupt_workspace_and_resumes() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-bad-workspace-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);

        let workspace_path = astra_services::session_workspace::workspace_dir_for(&session_id)
            .join("workspace.yaml");
        std::fs::write(&workspace_path, ":\nnot-valid-yaml").unwrap();

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            ..Default::default()
        };
        let mut state = SessionState {
            session_id: Some("existing-session".into()),
            turn: 7,
            history: vec![("old".into(), "state".into())],
            discovered_skills: ["stale-skill".to_string()].into_iter().collect(),
            ..Default::default()
        };

        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("corrupt workspace should be rebuilt, not block resume");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 2);
        assert!(state.discovered_skills.is_empty());
        let warning = state
            .session_persistence_error
            .as_deref()
            .expect("rebuilt workspace should surface a degradation warning");
        assert!(
            warning.contains("workspace metadata unreadable during resume"),
            "{warning}"
        );

        let rebuilt = session_workspace::read_workspace(&session_id)
            .expect("resume should rewrite readable workspace metadata");
        assert_eq!(rebuilt.session_id, session_id);
        assert_eq!(rebuilt.turn_count, 2);
        assert!(
            rebuilt
                .last_persistence_error
                .as_deref()
                .is_some_and(|error| error.contains("workspace metadata unreadable during resume")),
            "{rebuilt:?}"
        );
        let backup_count = std::fs::read_dir(workspace_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("workspace.yaml.corrupt-")
            })
            .count();
        assert_eq!(backup_count, 1, "corrupt workspace should be preserved");
    }

    #[test]
    #[serial_test::serial]
    fn workspace_summary_line_marks_invalid_workspace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-bad-summary-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 1);
        let workspace_path =
            astra_services::session_workspace::workspace_file_path(&session_id).unwrap();
        std::fs::write(&workspace_path, ":\nnot-valid-yaml").unwrap();

        let summary = workspace_summary_line(&session_id);
        assert!(
            summary.contains("workspace metadata unreadable"),
            "{summary}"
        );
        assert!(!summary.contains("not-valid-yaml"), "{summary}");
        assert!(summary.contains("1 turn"), "{summary}");
    }

    #[test]
    #[serial_test::serial]
    fn workspace_summary_line_journal_only_includes_turn_count() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-journal-only-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let workspace_path =
            astra_services::session_workspace::workspace_file_path(&session_id).unwrap();
        std::fs::remove_file(&workspace_path).unwrap();

        let summary = workspace_summary_line(&session_id);
        assert!(summary.contains("workspace metadata missing"), "{summary}");
        assert!(summary.contains("1 turn"), "{summary}");
    }

    #[test]
    fn resume_persistence_warning_formats_user_visible_notice() {
        let warning =
            resume_persistence_warning(Some("failed to append turn event: Is a directory"))
                .expect("warning");
        assert!(
            warning.contains("Session persistence degraded"),
            "{warning}"
        );
        assert!(warning.contains("failed to append turn event"), "{warning}");
        assert!(resume_persistence_warning(None).is_none());
        assert!(resume_persistence_warning(Some("   ")).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn workspace_summary_line_marks_persistence_degraded() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-degraded-summary-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let mut workspace = session_workspace::read_workspace(&session_id).unwrap();
        workspace.last_persistence_error = Some("failed to append turn event".to_string());
        session_workspace::write_workspace(&workspace).unwrap();

        let summary = workspace_summary_line(&session_id);
        assert!(summary.contains("persistence degraded"), "{summary}");
        assert!(summary.contains("2 turns"), "{summary}");
    }

    #[test]
    #[serial_test::serial]
    fn filter_session_list_entries_tracks_missing_and_invalid_workspace_skips() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let valid_session = format!("session-list-valid-{}", uuid::Uuid::new_v4());
        let invalid_session = format!("session-list-invalid-{}", uuid::Uuid::new_v4());
        let missing_session = format!("session-list-missing-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&valid_session, 2);
        write_local_resumable_session(&invalid_session, 1);
        write_local_resumable_session(&missing_session, 1);

        let invalid_workspace =
            astra_services::session_workspace::workspace_file_path(&invalid_session).unwrap();
        std::fs::write(&invalid_workspace, ":\nnot-valid-yaml").unwrap();
        let missing_workspace =
            astra_services::session_workspace::workspace_file_path(&missing_session).unwrap();
        std::fs::remove_file(&missing_workspace).unwrap();

        let session_ids = vec![
            valid_session.clone(),
            invalid_session.clone(),
            missing_session.clone(),
        ];
        let mut entries = build_session_list_entries(&session_ids);
        let outcome = filter_session_list_entries(
            &mut entries,
            &SessionListFilterOptions {
                filter_active: true,
                ..Default::default()
            },
            &std::env::current_dir().unwrap().display().to_string(),
            None,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sid, valid_session);
        assert_eq!(
            outcome,
            SessionListFilterOutcome {
                skipped_missing_workspace: 1,
                skipped_invalid_workspace: 1,
                project_filter_ignored: false,
            }
        );
        let warning = outcome
            .workspace_filter_warning()
            .expect("warning should describe skipped sessions");
        assert!(
            warning.contains("1 missing workspace metadata"),
            "{warning}"
        );
        assert!(
            warning.contains("1 unreadable workspace metadata"),
            "{warning}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn filter_session_list_entries_searches_invalid_workspace_hint() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let invalid_session = format!("session-list-search-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&invalid_session, 1);
        let workspace_path =
            astra_services::session_workspace::workspace_file_path(&invalid_session).unwrap();
        std::fs::write(&workspace_path, ":\nnot-valid-yaml").unwrap();

        let mut entries = build_session_list_entries(std::slice::from_ref(&invalid_session));
        let outcome = filter_session_list_entries(
            &mut entries,
            &SessionListFilterOptions {
                search_term: Some("metadata unreadable".to_string()),
                ..Default::default()
            },
            "",
            None,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sid, invalid_session);
        assert_eq!(outcome, SessionListFilterOutcome::default());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_fails_when_local_step_checkpoint_is_invalid_without_fallback() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-bad-step-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);
        write_invalid_local_step_checkpoint(&session_id, 2);

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            ..Default::default()
        };
        let mut state = SessionState {
            session_id: Some("existing-session".into()),
            turn: 7,
            history: vec![("old".into(), "state".into())],
            ..Default::default()
        };

        let error = apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect_err("invalid checkpoint without fallback should fail restore");

        assert!(
            error.contains("Failed to restore local step checkpoint"),
            "{error}"
        );
        assert_eq!(state.session_id.as_deref(), Some("existing-session"));
        assert_eq!(state.turn, 7);
        assert_eq!(
            state.history,
            vec![("old".to_string(), "state".to_string())]
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_surfaces_unreadable_local_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-bad-journal-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        std::fs::create_dir_all(session_journal::journal_file_path(&session_id)).unwrap();

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            restored_from_cloud: false,
            ..Default::default()
        };
        let mut state = SessionState::default();
        let error = apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect_err("unreadable local journal should abort restore");

        assert!(error.contains("failed to read session journal"), "{error}");
        assert!(!error.contains("not found or not owned"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_prepared_fork_restore_requires_existing_child_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();

        let error =
            match load_prepared_fork_restore("parent-session", "missing-child-session", 1).await {
                Ok(_) => panic!("missing child journal should abort fork restore"),
                Err(error) => error,
            };

        assert!(error.contains("missing session journal"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_prepared_fork_restore_verifies_the_service_owned_fork_basis() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let parent_id = format!("fork-parent-{}", uuid::Uuid::new_v4());
        let child_id = format!("fork-child-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&parent_id, 1);
        let fork = astra_services::fork_local_session(astra_services::ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: Some(child_id.clone()),
            label: None,
            forked_after_turn: Some(1),
            data_branch: None,
            snapshot_spec: None,
        })
        .expect("service creates and verifies the child fork basis");

        load_prepared_fork_restore(&parent_id, &child_id, fork.forked_at_turn)
            .await
            .expect("CLI accepts a child whose active state matches its immutable basis");

        std::fs::write(
            session_workspace::workspace_dir_for(&child_id)
                .join("fork-basis-v1")
                .join("workspace.yaml"),
            b"tampered",
        )
        .unwrap();
        let error =
            match load_prepared_fork_restore(&parent_id, &child_id, fork.forked_at_turn).await {
                Ok(_) => panic!("CLI must reject tampered fork basis before activation"),
                Err(error) => error,
            };
        assert!(error.contains("content does not match"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_uses_cloud_fallback_when_local_step_checkpoint_is_invalid() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-cloud-fallback-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);
        write_invalid_local_step_checkpoint(&session_id, 2);
        let mut objective =
            serde_json::json!({"role": "user", "content": "repair session lifecycle"});
        astra_turn_types::mark_user_turn_semantics(
            &mut objective,
            astra_turn_types::UserTurnSemantics::new(
                astra_turn_types::ObjectiveRelation::Replace,
                None,
            ),
        );

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            conversation_messages: vec![
                objective,
                serde_json::json!({"role": "assistant", "content": "cloud fallback"}),
            ],
            interruption: Some(serde_json::json!({
                "kind": "context_overflow",
                "resumable": true,
                "has_checkpoint": true,
                "tool_calls_completed": 2,
                "turns_completed": 2,
                "remaining_turns": 1,
            })),
            compaction_state: Some(serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_was_insufficient": true,
            })),
            ..Default::default()
        };
        let mut state = SessionState::default();
        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("cloud fallback should keep resume working");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        let anchor = state.continuation_anchor.as_ref().expect("restored anchor");
        assert_eq!(
            anchor.objective_context,
            vec!["objective: repair session lifecycle"]
        );
        let guidance = state.resume_guidance.expect("resume guidance");
        assert!(
            guidance.contains("objective: repair session lifecycle"),
            "{guidance}"
        );
        assert!(guidance.contains("3 attempt(s)"), "{guidance}");
        assert_eq!(
            state.runtime_compaction_state,
            Some(serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_was_insufficient": true,
            }))
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_journal_history_if_available_does_not_overwrite_cloud_history() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-history-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 1);

        let mut state = SessionState::default();
        state.history = vec![("from-cloud".to_string(), "cloud-data".to_string())];

        restore_journal_history_if_available(&mut state, &session_id)
            .await
            .unwrap();

        // Cloud-restored history is preserved when non-empty; local journal is not used
        // to overwrite fresher cloud state.
        assert_eq!(
            state.history,
            vec![("from-cloud".to_string(), "cloud-data".to_string())]
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_journal_history_if_available_prefers_local_when_more_entries() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-more-{}", uuid::Uuid::new_v4());
        // Write a journal with multiple turn events so local has more history entries.
        let writer = session_journal::JournalWriter::new(&session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        for i in 1..=3 {
            writer
                .append(&session_journal::JournalEvent::turn(
                    Some(&session_id),
                    i,
                    Some("gpt-5"),
                    &format!("prompt-{i}"),
                    &format!("response-{i}"),
                    0,
                    10,
                    5,
                    5,
                ))
                .unwrap();
        }

        let mut state = SessionState::default();
        state.history = vec![("from-cloud".to_string(), "cloud-1".to_string())];

        restore_journal_history_if_available(&mut state, &session_id)
            .await
            .unwrap();

        // Local journal wins when it has more entries (3 local vs 1 cloud).
        assert_eq!(
            state.history.len(),
            3,
            "local journal should win when it has more entries, got {} entries",
            state.history.len()
        );
    }

    // ── CSL resume tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn restore_from_csl_populates_history_and_state() {
        use astra_turn_core::conversation_log::{
            AppendMeta, CslEntry, CslStore, SessionStateCompact, file_store::FileCslStore,
        };
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-resume-{}", uuid::Uuid::new_v4());

        let store = FileCslStore::new(session_journal::local_owner_sessions_dir());
        let snapshot = CslEntry::Snapshot {
            seq: 0,
            turn: 1,
            messages: vec![
                serde_json::json!({"role": "user", "content": "hello"}),
                serde_json::json!({"role": "assistant", "content": "hi there"}),
            ],
            session_state: SessionStateCompact {
                recent_tools: vec!["bash".into(), "read_file".into()],
                activated_deferred_tool_names: vec!["write_file".into()],
                ..Default::default()
            },
        };
        store
            .append(&session_id, &snapshot, &AppendMeta::default())
            .await
            .unwrap();

        let delta = CslEntry::TurnDelta {
            seq: 1,
            turn: 2,
            appended: vec![
                serde_json::json!({"role": "user", "content": "what next?"}),
                serde_json::json!({"role": "assistant", "content": "let's continue"}),
            ],
            state_patch: None,
        };
        store
            .append(&session_id, &delta, &AppendMeta::default())
            .await
            .unwrap();

        let mut state = SessionState::default();
        restore_journal_history_if_available(&mut state, &session_id)
            .await
            .unwrap();

        assert_eq!(state.history.len(), 2, "should have 2 user/assistant pairs");
        assert_eq!(state.history[0].0, "hello");
        assert_eq!(state.history[0].1, "hi there");
        assert_eq!(state.history[1].0, "what next?");
        assert_eq!(state.history[1].1, "let's continue");
        assert!(state.csl_manager.is_some(), "CSL manager should be set");
        assert_eq!(
            state.recent_tools,
            vec!["bash".to_string(), "read_file".to_string()],
            "should restore recent_tools from snapshot state"
        );
        assert_eq!(
            state.activated_deferred_tool_names,
            vec!["write_file".to_string()],
            "should restore pending deferred activations from snapshot state"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_without_csl_falls_back_to_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("no-csl-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 3);

        let mut state = SessionState::default();
        restore_journal_history_if_available(&mut state, &session_id)
            .await
            .unwrap();

        assert!(
            !state.history.is_empty(),
            "should fall back to journal history"
        );
        assert!(
            state.csl_manager.is_none(),
            "CSL manager should remain None"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_without_csl_restores_recent_tools_from_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("no-csl-tools-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&session_id),
                    1,
                    Some("gpt-5"),
                    "continue",
                    "restored",
                    0,
                    10,
                    5,
                    5,
                )
                .with_tool_surface(
                    vec![],
                    vec![],
                    vec!["bash".into(), "grep".into()],
                    0,
                ),
            )
            .unwrap();

        let mut state = SessionState::default();
        restore_journal_history_if_available(&mut state, &session_id)
            .await
            .unwrap();

        assert_eq!(
            state.recent_tools,
            vec!["bash".to_string(), "grep".to_string()]
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_from_corrupt_csl_returns_error_instead_of_falling_back() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("corrupt-csl-{}", uuid::Uuid::new_v4());
        let store = astra_services::local_session_artifact_store();
        let session_dir = astra_services::SessionArtifactStore::session_dir(&store, &session_id)
            .expect("owner-bound test session dir");
        std::fs::create_dir_all(&session_dir).unwrap();
        write_local_resumable_session(&session_id, 2);
        std::fs::write(
            session_dir.join("conversation_log.jsonl"),
            "{\"type\":\"snapshot\",\"seq\":1,\"turn\":1,\"messages\":[]\n{\"type\":\"snapshot\",\"seq\":2,\"turn\":1,\"messages\":[],\"session_state\":{}}\n",
        )
        .unwrap();

        let mut state = SessionState::default();
        let error = restore_journal_history_if_available(&mut state, &session_id)
            .await
            .expect_err("corrupt csl should fail");

        assert!(error.contains("load CSL state"), "{error}");
        assert!(
            state.history.is_empty(),
            "lossy journal fallback should not run"
        );
    }

    // ── derive_history_pairs_from_messages tests ─────────────────────────

    #[test]
    fn derive_history_pairs_simple_conversation() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "q1"}),
            serde_json::json!({"role": "assistant", "content": "a1"}),
            serde_json::json!({"role": "user", "content": "q2"}),
            serde_json::json!({"role": "assistant", "content": "a2"}),
        ];
        let pairs =
            crate::cli::session::session_continuation::history_pairs_from_messages(&messages);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("q1".into(), "a1".into()));
        assert_eq!(pairs[1], ("q2".into(), "a2".into()));
    }

    #[test]
    fn derive_history_pairs_skips_tool_messages() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "fix the bug"}),
            serde_json::json!({"role": "assistant", "content": "I'll read the file"}),
            serde_json::json!({"role": "tool", "content": "file contents here"}),
            serde_json::json!({"role": "assistant", "content": "done, fixed it"}),
            serde_json::json!({"role": "user", "content": "thanks"}),
            serde_json::json!({"role": "assistant", "content": "you're welcome"}),
        ];
        let pairs =
            crate::cli::session::session_continuation::history_pairs_from_messages(&messages);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "fix the bug");
        assert_eq!(pairs[0].1, "I'll read the file\n\ndone, fixed it");
        assert_eq!(pairs[1].0, "thanks");
        assert_eq!(pairs[1].1, "you're welcome");
    }

    #[test]
    fn derive_history_pairs_empty_messages() {
        let pairs = crate::cli::session::session_continuation::history_pairs_from_messages(&[]);
        assert!(pairs.is_empty());
    }

    #[test]
    fn derive_history_pairs_user_only_no_assistant() {
        let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let pairs =
            crate::cli::session::session_continuation::history_pairs_from_messages(&messages);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("hello".into(), String::new()));
    }

    #[test]
    fn derive_history_pairs_structured_content_preserves_text() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "question"}),
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer"}]}),
        ];
        let pairs =
            crate::cli::session::session_continuation::history_pairs_from_messages(&messages);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "question");
        assert_eq!(pairs[0].1, "answer");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_prefers_local_state_over_stale_remote_preflight() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-stale-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 3);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "detail": "Session not found"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect(
                "local journal/workspace should restore even when cloud no longer has the session",
            );

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 3);
        assert_eq!(
            crate::cli::cli_config::cli_utils::load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(session_id.as_str())
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_clears_cloud_only_stale_pointer() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-cloud-stale-{}", uuid::Uuid::new_v4());
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{session_id}/resume")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "detail": "Session not found"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "detail": "Session not found"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        let err = restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect_err("cloud-only stale session should fail");

        assert!(err.contains("has no local resumable state"), "got: {err}");
        assert_eq!(state.session_id, None);
        assert_eq!(
            crate::cli::cli_config::cli_utils::load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            None
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn list_cloud_resumable_sessions_uses_server_restore_payload() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _token_guard = EnvGuard::set("ASTRA_ACCESS_TOKEN", "test-token");

        let session_id = format!("cloud-list-{}", uuid::Uuid::new_v4());
        let workspace = session_workspace::WorkspaceMetadata::with_context(
            &session_id,
            "gpt-5",
            "/srv/cloud-project",
            Some("feature/cloud"),
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sessions": [{
                    "session_id": session_id,
                    "turn_count": 5,
                    "total_tokens_in": 120,
                    "total_tokens_out": 45,
                    "total_cache_read_tokens": 22,
                    "total_cache_creation_tokens": 7,
                    "recent_tools": ["bash", "grep"],
                    "checkpoint_count": 2,
                    "last_status": "active",
                    "git_branch": "feature/cloud",
                    "model": "gpt-5",
                    "title": "Cloud only session",
                    "restored_from_cloud": true,
                    "workspace": workspace,
                }],
                "limit": 20
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let sessions = session_restore_client::list_cloud_resumable_sessions(None, &api)
            .await
            .expect("cloud resumable list");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
        assert_eq!(sessions[0].turn_count, 5);
        assert!(sessions[0].restored_from_cloud);
        assert_eq!(
            sessions[0]
                .workspace
                .as_ref()
                .map(|workspace| workspace.cwd.as_str()),
            Some("/srv/cloud-project")
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_resumable_session_candidates_keeps_cloud_results_when_local_scan_fails() {
        let _creds_guard = crate::tests::isolate_credentials();
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("sessions-root");
        let _guard = JournalDirGuard::new(&sessions_root);
        let broken_owner_root = session_journal::local_owner_sessions_dir();
        std::fs::create_dir_all(broken_owner_root.parent().unwrap()).unwrap();
        std::fs::write(&broken_owner_root, "not-a-directory").unwrap();
        write_profile_with_token("placeholder-session");

        let session_id = format!("cloud-only-{}", uuid::Uuid::new_v4());
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sessions": [{
                    "session_id": session_id,
                    "turn_count": 4,
                    "total_tokens_in": 0,
                    "total_tokens_out": 0,
                    "last_status": "active",
                    "recent_tools": [],
                    "checkpoint_count": 0,
                    "restored_from_cloud": true
                }],
                "limit": 20
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let candidates = load_resumable_session_candidates(None, &api, 20)
            .await
            .expect("cloud sessions should still load");

        assert_eq!(candidates.sessions.len(), 1);
        assert_eq!(candidates.sessions[0].session_id, session_id);
        assert!(candidates.local_scan_error.is_some());
        assert!(candidates.cloud_scan_error.is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_resumable_session_candidates_errors_when_local_scan_fails_without_cloud() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("sessions-root");
        let _guard = JournalDirGuard::new(&sessions_root);
        let broken_owner_root = session_journal::local_owner_sessions_dir();
        std::fs::create_dir_all(broken_owner_root.parent().unwrap()).unwrap();
        std::fs::write(&broken_owner_root, "not-a-directory").unwrap();
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();

        let error = load_resumable_session_candidates(None, &api, 20)
            .await
            .expect_err("local scan failure should surface when cloud is unavailable");

        assert!(error.contains("failed to scan local sessions"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn switch_session_into_state_restores_workspace_scoped_state() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("switch-restore-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);

        let mut ws = session_workspace::read_workspace(&session_id).unwrap();
        ws.discovered_skills = vec!["session-recovery".to_string()];
        ws.last_scenario_change_turn = Some(2);
        ws.last_token_budget_direction = 1;
        ws.last_persistence_error = Some("failed to append turn event".to_string());
        session_workspace::write_workspace(&ws).unwrap();

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState {
            session_id: Some("current-session".into()),
            history: vec![("old".into(), "state".into())],
            discovered_skills: ["obsolete".to_string()].into_iter().collect(),
            ..Default::default()
        };
        state.set_session_id("current-session");
        let old_created = state
            .task_manager
            .create(&serde_json::json!({"title": "old session task"}))
            .await;
        assert!(old_created.contains("created"), "{old_created}");
        let restored_task_manager = astra_tools::task_mgmt::TaskManager::new(
            session_id.clone(),
            state.task_manager.store(),
        );
        let restored_created = restored_task_manager
            .create(&serde_json::json!({"title": "restored session task"}))
            .await;
        assert!(restored_created.contains("created"), "{restored_created}");

        switch_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect("switch should reuse strict resume restore");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 2);
        assert_eq!(
            state.history,
            vec![("continue".to_string(), "restored".to_string())]
        );
        assert!(state.discovered_skills.contains("session-recovery"));
        assert_eq!(
            state.session_persistence_error.as_deref(),
            Some("failed to append turn event")
        );
        assert!(state.pending_adaptive_state.is_none());
        assert!(
            state.journal.is_some(),
            "switch should initialize a journal"
        );
        let task_list = state
            .task_manager
            .list(&serde_json::json!({"status_filter": "all"}))
            .await;
        assert!(
            task_list.contains("restored session task"),
            "switch must rebind task manager to restored session: {task_list}"
        );
        assert!(
            !task_list.contains("old session task"),
            "switch must not leave task manager bound to old session: {task_list}"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_restores_cloud_only_session_and_workspace_metadata() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-cloud-only-{}", uuid::Uuid::new_v4());
        write_profile_with_token(&session_id);

        let mut workspace = session_workspace::WorkspaceMetadata::with_context(
            &session_id,
            "gpt-5",
            "/srv/cloud-project",
            Some("feature/cloud"),
        );
        workspace.git_root = Some("/srv/cloud-project".to_string());
        workspace.git_head = Some("abc1234".to_string());
        workspace.turn_count = 3;
        workspace.total_tokens_in = 120;
        workspace.total_tokens_out = 45;
        workspace.total_cache_read_tokens = 22;
        workspace.total_cache_creation_tokens = 7;
        workspace.status = "active".to_string();
        workspace.discovered_skills = vec!["cloud-recovery".to_string()];
        workspace.last_scenario_change_turn = Some(3);
        workspace.last_token_budget_direction = 1;
        workspace.last_persistence_error = Some("failed to write workspace metadata".to_string());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{session_id}/resume")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "turn_count": 3,
                "total_tokens_in": 120,
                "total_tokens_out": 45,
                "total_cache_read_tokens": 22,
                "total_cache_creation_tokens": 7,
                "recent_tools": ["bash", "grep"],
                "checkpoint_count": 0,
                "last_status": "active",
                "git_branch": "feature/cloud",
                "model": "gpt-5",
                "restored_from_cloud": true,
                "workspace": workspace,
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect("cloud-only restore should succeed");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 3);
        assert_eq!(state.total_prompt_tokens, 120);
        assert_eq!(state.total_completion_tokens, 45);
        assert_eq!(state.total_cache_read_tokens, 22);
        assert_eq!(state.total_cache_creation_tokens, 7);
        assert_eq!(
            state.discovered_skills,
            ["cloud-recovery".to_string()].into_iter().collect()
        );
        assert_eq!(
            state.session_persistence_error.as_deref(),
            Some("failed to write workspace metadata")
        );
        assert!(state.journal.is_some());

        let persisted = session_workspace::read_workspace(&session_id)
            .expect("cloud resume should persist workspace metadata");
        assert_eq!(persisted.cwd, "/srv/cloud-project");
        assert_eq!(persisted.git_root.as_deref(), Some("/srv/cloud-project"));
        assert_eq!(persisted.git_branch.as_deref(), Some("feature/cloud"));
        assert_eq!(persisted.git_head.as_deref(), Some("abc1234"));
        assert_eq!(persisted.turn_count, 3);
        assert_eq!(persisted.total_cache_read_tokens, 22);
        assert_eq!(
            persisted.discovered_skills,
            vec!["cloud-recovery".to_string()]
        );
        assert_eq!(
            persisted.last_persistence_error.as_deref(),
            Some("failed to write workspace metadata")
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_restores_live_remote_session_from_local_workspace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-live-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 2);
        assert_eq!(state.total_prompt_tokens, 15);
        assert_eq!(state.total_completion_tokens, 7);
        assert_eq!(state.total_cache_read_tokens, 9);
        assert_eq!(state.total_cache_creation_tokens, 3);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_uses_remote_cache_totals_without_local_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        let session_id = format!("resume-remote-cache-{}", uuid::Uuid::new_v4());

        let restored = astra_services::session_restore::RestoredSession {
            session_id: session_id.clone(),
            turn_count: 4,
            total_tokens_in: 120,
            total_tokens_out: 30,
            total_cache_read_tokens: 44,
            total_cache_creation_tokens: 11,
            last_status: "active".to_string(),
            restored_from_cloud: true,
            ..Default::default()
        };

        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("remote restore should succeed without local journal");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.total_prompt_tokens, 120);
        assert_eq!(state.total_completion_tokens, 30);
        assert_eq!(state.total_cache_read_tokens, 44);
        assert_eq!(state.total_cache_creation_tokens, 11);

        let workspace = astra_services::session_workspace::read_workspace(&session_id)
            .expect("resume should recreate workspace metadata");
        assert_eq!(workspace.turn_count, 4);
        assert_eq!(workspace.total_cache_read_tokens, 44);
        assert_eq!(workspace.total_cache_creation_tokens, 11);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_merges_session_memory_into_anchor() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-memory-anchor-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        write_profile_with_token(&session_id);

        let memory_body = "# Session Memory

## Active Goals
- Improve prompt cache behavior

## Pending Todos
- Add shutdown flush

## Current State
- Resume should carry session memory forward

## Completed
- Removed legacy memory extraction
";
        let encoded = astra_runtime::session_memory::runner::encode_session_memory_entry(
            &session_id,
            memory_body,
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory/retrieve"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "memories": [{
                    "memory_id": "mem-1",
                    "content": encoded,
                    "memory_type": "working",
                    "session_id": session_id
                }]
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        let anchor = state.continuation_anchor.expect("continuation anchor");
        assert!(anchor.contains("[Session memory recap]"), "{anchor}");
        assert!(anchor.contains("Improve prompt cache behavior"), "{anchor}");
        assert!(anchor.contains("Add shutdown flush"), "{anchor}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_recovers_journal_model_over_workspace_default() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-default-model-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let mut ws = session_workspace::read_workspace(&session_id).unwrap();
        ws.model = Some("default".to_string());
        session_workspace::write_workspace(&ws).unwrap();
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState {
            model: Some("default".to_string()),
            ..SessionState::default()
        };
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.model.as_deref(), Some("gpt-5"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_restores_permission_mode() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-mode-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let mut ws = session_workspace::read_workspace(&session_id).unwrap();
        ws.permission_mode = Some("plan".to_string());
        session_workspace::write_workspace(&ws).unwrap();
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        state.perm_manager.set_mode(PermissionMode::Auto);
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.perm_manager.mode(), PermissionMode::Plan);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_rejects_invalid_permission_mode_without_rebinding() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-invalid-mode-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let mut ws = session_workspace::read_workspace(&session_id).unwrap();
        ws.permission_mode = Some("yolo".to_string());
        session_workspace::write_workspace(&ws).unwrap();
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState {
            session_id: Some("current-session".into()),
            model: Some("gpt-4o".into()),
            ..SessionState::default()
        };
        state.perm_manager.set_mode(PermissionMode::Auto);

        let error = restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect_err("invalid persisted permission mode must fail restore");

        assert!(
            error.contains("invalid persisted permission mode"),
            "{error}"
        );
        assert_eq!(state.session_id.as_deref(), Some("current-session"));
        assert_eq!(state.model.as_deref(), Some("gpt-4o"));
        assert_eq!(state.perm_manager.mode(), PermissionMode::Auto);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_surfaces_interrupted_plan_and_durable_lifecycle_summary() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-lifecycle-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        write_local_step_checkpoint_with_interruption(&session_id, 2);
        write_workspace_lifecycle_state(&session_id);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        assert!(state.last_turn_interrupted);
        assert!(state.resume_guidance.is_some());
        assert_eq!(
            state.executing_plan_goal.as_deref(),
            Some("Ship lifecycle UX")
        );
        assert_eq!(state.plan_execution_rounds, 4);
        assert_eq!(
            state.plan_execution_corrections,
            vec!["tighten resume messaging"]
        );
        assert!(state.durable_task_state.is_some());

        let summary =
            crate::cli::execution_state_summary::format_for_session_state(&state).unwrap();
        assert!(summary.contains("turn state: last turn was interrupted"));
        assert!(summary.contains("plan execution: goal=\"Ship lifecycle UX\""));
        assert!(summary.contains("in_progress=\"Verify lifecycle state\""));
        assert!(summary.contains("rounds=4"));
        assert!(summary.contains("corrections=1"));
        assert!(summary.contains("durable verification: status=active"));
        assert!(summary.contains("verified=1/2"));
        assert!(summary.contains("subtask=\"Verify restore contract\""));
        assert!(summary.contains("stage=awaiting_verification"));
    }

    // ── Fork CSL integration tests ──────────────────────────────────────

    #[tokio::test]
    async fn session_fork_creates_child_csl_snapshot() {
        use astra_turn_core::conversation_log::{
            AppendMeta, CslEntry, CslStore, SessionStateCompact, file_store::FileCslStore,
            manager::CslManager,
        };
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let parent_id = format!("fork-parent-{}", uuid::Uuid::new_v4());
        let child_id = format!("fork-child-{}", uuid::Uuid::new_v4());

        let base_dir = session_journal::local_owner_sessions_dir();
        let store = std::sync::Arc::new(FileCslStore::new(base_dir));

        // Write a 2-turn parent CSL.
        let snapshot = CslEntry::Snapshot {
            seq: 1,
            turn: 1,
            messages: vec![
                serde_json::json!({"role": "user", "content": "hello"}),
                serde_json::json!({"role": "assistant", "content": "hi"}),
            ],
            session_state: SessionStateCompact {
                recent_tools: vec!["bash".into()],
                ..Default::default()
            },
        };
        store
            .append(&parent_id, &snapshot, &AppendMeta::default())
            .await
            .unwrap();

        let delta = CslEntry::TurnDelta {
            seq: 2,
            turn: 2,
            appended: vec![
                serde_json::json!({"role": "user", "content": "next"}),
                serde_json::json!({"role": "assistant", "content": "ok"}),
            ],
            state_patch: None,
        };
        store
            .append(&parent_id, &delta, &AppendMeta::default())
            .await
            .unwrap();

        // Fork parent → child at turn 2.
        let parent_mgr =
            CslManager::new(store.clone(), parent_id.clone(), Default::default()).unwrap();
        let (child_mgr, child_mat) = parent_mgr.fork(&child_id, 2).await.unwrap();

        // Child should have last_seq=1, last_turn=2.
        assert_eq!(child_mgr.last_seq(), 1);
        assert_eq!(child_mgr.last_turn(), 2);

        // fork() returns MaterializedState directly — no need for double load.
        let mat = child_mat.expect("child should have CSL data");
        assert_eq!(mat.messages.len(), 4);
        assert_eq!(mat.messages[0]["content"], "hello");
        assert_eq!(mat.messages[3]["content"], "ok");
    }

    #[tokio::test]
    async fn session_fork_child_resume_uses_csl() {
        use astra_turn_core::conversation_log::{
            AppendMeta, CslEntry, CslStore, SessionStateCompact, file_store::FileCslStore,
            manager::CslManager,
        };
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let parent_id = format!("fork-resume-parent-{}", uuid::Uuid::new_v4());
        let child_id = format!("fork-resume-child-{}", uuid::Uuid::new_v4());

        let base_dir = session_journal::local_owner_sessions_dir();
        let store = std::sync::Arc::new(FileCslStore::new(base_dir));

        let snapshot = CslEntry::Snapshot {
            seq: 1,
            turn: 1,
            messages: vec![
                serde_json::json!({"role": "user", "content": "q1"}),
                serde_json::json!({"role": "assistant", "content": "a1"}),
            ],
            session_state: SessionStateCompact {
                recent_tools: vec!["read_file".into()],
                activated_deferred_tool_names: vec!["write_file".into()],
                ..Default::default()
            },
        };
        store
            .append(&parent_id, &snapshot, &AppendMeta::default())
            .await
            .unwrap();

        // Fork and get child manager.
        let parent_mgr =
            CslManager::new(store.clone(), parent_id.clone(), Default::default()).unwrap();
        let (_child_mgr, _) = parent_mgr.fork(&child_id, 1).await.unwrap();

        // Simulate resume: restore_journal_history_if_available should pick up
        // the child's CSL data (not fall back to journal).
        let mut state = SessionState::default();
        restore_journal_history_if_available(&mut state, &child_id)
            .await
            .unwrap();

        assert!(state.csl_manager.is_some(), "should use CSL path");
        assert_eq!(state.history.len(), 1, "should have 1 user/assistant pair");
        assert_eq!(state.history[0].0, "q1");
        assert_eq!(state.history[0].1, "a1");
        assert_eq!(
            state.recent_tools,
            vec!["read_file".to_string()],
            "should restore recent_tools from CSL"
        );
        assert_eq!(
            state.activated_deferred_tool_names,
            vec!["write_file".to_string()],
            "should restore pending deferred activations from CSL"
        );
    }

    #[tokio::test]
    async fn session_fork_no_parent_csl_gracefully_skips() {
        use astra_turn_core::conversation_log::{file_store::FileCslStore, manager::CslManager};
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let parent_id = format!("fork-no-csl-{}", uuid::Uuid::new_v4());
        let child_id = format!("fork-no-csl-child-{}", uuid::Uuid::new_v4());

        let base_dir = session_journal::local_owner_sessions_dir();
        let store = std::sync::Arc::new(FileCslStore::new(base_dir));

        // Parent has no CSL data. fork() succeeds but writes nothing to child.
        let parent_mgr =
            CslManager::new(store.clone(), parent_id.clone(), Default::default()).unwrap();
        let (_child_mgr, _) = parent_mgr.fork(&child_id, 0).await.unwrap();

        // Child has no CSL file, so resume falls back to journal (which is also
        // empty). No error should occur.
        let mut state = SessionState::default();
        restore_journal_history_if_available(&mut state, &child_id)
            .await
            .unwrap();

        assert!(
            state.csl_manager.is_none(),
            "no CSL data written, manager stays None"
        );
        assert!(
            state.history.is_empty(),
            "no history from either CSL or journal"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn fork_state_snapshot_capture_restore_roundtrip_preserves_fields() {
        use astra_turn_core::conversation_log::{
            SessionStateCompact, file_store::FileCslStore, manager::CslManager,
        };

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("fork-snapshot-{}", uuid::Uuid::new_v4());
        let store = std::sync::Arc::new(FileCslStore::new(
            session_journal::local_owner_sessions_dir(),
        ));
        let mut mgr = CslManager::new(store, sid.clone(), Default::default()).unwrap();
        mgr.persist_turn(
            1,
            &[serde_json::json!({"role": "user", "content": "hi"})],
            &SessionStateCompact {
                recent_tools: vec!["bash".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut source = SessionState::default();
        source.set_session_id(sid.clone());
        session_startup::initialize_journal_pub(&mut source, &sid);
        source.turn = 4;
        source.total_prompt_tokens = 11;
        source.total_completion_tokens = 22;
        source.total_cache_read_tokens = 33;
        source.total_cache_creation_tokens = 44;
        source.last_turn_event = Some(session_journal::JournalEvent::turn(
            Some(&sid),
            4,
            Some("gpt-5"),
            "question",
            "answer",
            0,
            66,
            40,
            26,
        ));
        source.run_id = Some("run-1".into());
        source.history = vec![("q1".into(), "a1".into())];
        source.recent_tools = vec!["bash".into(), "read_file".into()];
        source.last_response = Some("a1".into());
        source.continuation_anchor = Some(ContinuationAnchor::rendered_for_test("anchor"));
        source.csl_manager = Some(mgr);

        let snapshot = ForkStateSnapshot::capture(&mut source);

        let mut restored = SessionState::default();
        snapshot.restore(&mut restored);

        assert_eq!(restored.session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(restored.turn, 4);
        assert_eq!(restored.total_prompt_tokens, 11);
        assert_eq!(restored.total_completion_tokens, 22);
        assert_eq!(restored.total_cache_read_tokens, 33);
        assert_eq!(restored.total_cache_creation_tokens, 44);
        assert_eq!(restored.run_id.as_deref(), Some("run-1"));
        assert_eq!(restored.history, vec![("q1".to_string(), "a1".to_string())]);
        assert_eq!(
            restored.recent_tools,
            vec!["bash".to_string(), "read_file".to_string()]
        );
        assert_eq!(restored.last_response.as_deref(), Some("a1"));
        assert_eq!(restored.continuation_anchor.as_deref(), Some("anchor"));
        assert!(
            restored.journal.is_some(),
            "restore should reinitialize journal"
        );
        assert_eq!(
            restored
                .csl_manager
                .as_ref()
                .expect("csl manager restored")
                .last_seq(),
            1
        );
    }

    #[test]
    fn fork_state_snapshot_restore_without_session_id_clears_identity() {
        let mut source = SessionState {
            turn: 2,
            history: vec![("q".into(), "a".into())],
            recent_tools: vec!["bash".into()],
            last_response: Some("a".into()),
            continuation_anchor: Some(ContinuationAnchor::rendered_for_test("anchor")),
            ..Default::default()
        };
        let snapshot = ForkStateSnapshot::capture(&mut source);

        let mut state = SessionState::default();
        state.set_session_id("existing-session");
        session_startup::initialize_journal_pub(&mut state, "existing-session");
        state.turn = 9;
        state.history = vec![("stale".into(), "state".into())];
        state.recent_tools = vec!["stale-tool".into()];
        state.last_response = Some("stale".into());
        state.continuation_anchor = Some(ContinuationAnchor::rendered_for_test("stale-anchor"));

        snapshot.restore(&mut state);

        assert!(state.session_id.is_none());
        assert!(state.journal.is_none());
        assert_eq!(state.turn, 2);
        assert_eq!(state.history, vec![("q".to_string(), "a".to_string())]);
        assert_eq!(state.recent_tools, vec!["bash".to_string()]);
        assert_eq!(state.last_response.as_deref(), Some("a"));
        assert_eq!(state.continuation_anchor.as_deref(), Some("anchor"));
    }

    #[tokio::test]
    async fn fork_state_guard_restores_original_state_on_drop_without_commit() {
        let mut state = SessionState::default();
        state.set_session_id("parent-session");
        session_startup::initialize_journal_pub(&mut state, "parent-session");
        state.turn = 3;
        state.history = vec![("q1".into(), "a1".into())];
        state.recent_tools = vec!["bash".into()];
        state.last_response = Some("a1".into());
        state.continuation_anchor = Some(ContinuationAnchor::rendered_for_test("anchor"));

        {
            let mut guard = ForkStateGuard::new(&mut state);
            let child_state = session_runtime::RestoredSessionState {
                history: vec![("child-q".into(), "child-a".into())],
                turn: 1,
                recent_tools: vec!["read_file".into()],
                total_prompt_tokens: 10,
                total_completion_tokens: 20,
                total_cache_read_tokens: 30,
                total_cache_creation_tokens: 40,
            };
            let restored_child = PreparedForkRestore {
                history: child_state.history.clone(),
                recent_tools: child_state.recent_tools.clone(),
                activated_deferred_tool_names: Vec::new(),
                csl_manager: None,
                journal_state: child_state,
                last_turn_event: None,
            };
            let outcome = apply_prepared_fork_restore(
                guard.state(),
                "parent-session",
                "child-session",
                restored_child,
                None,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ForkTaskBoardRestore::Copied);
        }

        assert_eq!(state.session_id.as_deref(), Some("parent-session"));
        assert_eq!(state.turn, 3);
        assert_eq!(state.history, vec![("q1".to_string(), "a1".to_string())]);
        assert_eq!(state.recent_tools, vec!["bash".to_string()]);
        assert_eq!(state.last_response.as_deref(), Some("a1"));
        assert_eq!(state.continuation_anchor.as_deref(), Some("anchor"));
    }

    #[tokio::test]
    async fn fork_state_guard_restores_root_mailbox_on_drop_without_commit() {
        let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = std::sync::Arc::new(
            astra_runtime::server::delegation::engine::DelegationTracker::new(),
        );
        let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
            transport.clone(),
            tracker,
        ));
        let root_addr = astra_messaging::AgentAddress::new("parent-session", "main");

        let mut state = SessionState::default();
        state.set_session_id("parent-session");
        state.root_mailbox = Some(router.register(root_addr.clone(), None).await.unwrap());

        {
            let mut guard = ForkStateGuard::new(&mut state);
            guard.state().set_session_id("child-session");
        }

        assert_eq!(state.session_id.as_deref(), Some("parent-session"));
        assert_eq!(
            state.root_mailbox.as_ref().map(|mailbox| &mailbox.address),
            Some(&root_addr)
        );
        assert_eq!(transport.agent_count().await, 1);

        state.unregister_root_mailbox().await;
        assert_eq!(transport.agent_count().await, 0);
        router
            .register(root_addr, None)
            .await
            .expect("explicit unregister should release restored root mailbox route");
    }

    #[tokio::test]
    async fn apply_prepared_fork_restore_copies_parent_task_board_to_child() {
        let mut state = SessionState::default();
        state.set_session_id("parent-session");
        let created = state
            .task_manager
            .create(&serde_json::json!({"title": "continue forked work"}))
            .await;
        assert!(created.contains("created"), "{created}");
        let started = state
            .task_manager
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(!started.starts_with("Error:"), "{started}");

        let child_state = session_runtime::RestoredSessionState {
            history: vec![("child-q".into(), "child-a".into())],
            turn: 1,
            recent_tools: vec!["task_board".into()],
            total_prompt_tokens: 10,
            total_completion_tokens: 20,
            total_cache_read_tokens: 30,
            total_cache_creation_tokens: 40,
        };
        let restored_child = PreparedForkRestore {
            history: child_state.history.clone(),
            recent_tools: child_state.recent_tools.clone(),
            activated_deferred_tool_names: Vec::new(),
            csl_manager: None,
            journal_state: child_state,
            last_turn_event: None,
        };

        let outcome = apply_prepared_fork_restore(
            &mut state,
            "parent-session",
            "child-session",
            restored_child,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ForkTaskBoardRestore::Copied);

        let child_list = state
            .task_manager
            .list(&serde_json::json!({"status_filter": "all"}))
            .await;
        assert!(
            child_list.contains("continue forked work"),
            "forked child should inherit the parent task board snapshot: {child_list}"
        );
        assert!(
            child_list.contains("\"status\":\"paused\""),
            "forked child should inherit active parent work as paused, not in_progress: {child_list}"
        );
        let child_snapshot = state.task_manager.snapshot().await.unwrap();
        assert_eq!(
            child_snapshot[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("fork_copied_from_status"))
                .and_then(serde_json::Value::as_str),
            Some("in_progress"),
            "forked child should explain active parent work was paused: {child_snapshot:?}"
        );
    }

    #[tokio::test]
    async fn apply_prepared_fork_restore_resets_task_reminder_counters() {
        let mut state = SessionState {
            turns_since_task_use: 9,
            turns_since_task_reminder: 9,
            ..SessionState::default()
        };
        state.set_session_id("parent-session");
        let created = state
            .task_manager
            .create(&serde_json::json!({"title": "continue forked work"}))
            .await;
        assert!(created.contains("created"), "{created}");

        let child_state = session_runtime::RestoredSessionState {
            history: vec![("child-q".into(), "child-a".into())],
            turn: 1,
            recent_tools: vec![],
            total_prompt_tokens: 10,
            total_completion_tokens: 20,
            total_cache_read_tokens: 30,
            total_cache_creation_tokens: 40,
        };
        let restored_child = PreparedForkRestore {
            history: child_state.history.clone(),
            recent_tools: child_state.recent_tools.clone(),
            activated_deferred_tool_names: Vec::new(),
            csl_manager: None,
            journal_state: child_state,
            last_turn_event: None,
        };

        apply_prepared_fork_restore(
            &mut state,
            "parent-session",
            "child-session",
            restored_child,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            state.turns_since_task_use, 0,
            "forked child must not inherit stale parent task-use reminder age"
        );
        assert_eq!(
            state.turns_since_task_reminder, 0,
            "forked child must not immediately inject a task reminder from parent counters"
        );
    }

    #[tokio::test]
    async fn forked_child_first_turn_does_not_inject_stale_parent_task_reminder() {
        let mut state = SessionState {
            turns_since_task_use: 9,
            turns_since_task_reminder: 9,
            ..SessionState::default()
        };
        state.set_session_id("parent-session");
        let created = state
            .task_manager
            .create(&serde_json::json!({"title": "continue forked work"}))
            .await;
        assert!(created.contains("created"), "{created}");

        let child_state = session_runtime::RestoredSessionState {
            history: vec![("child-q".into(), "child-a".into())],
            turn: 1,
            recent_tools: vec![],
            total_prompt_tokens: 10,
            total_completion_tokens: 20,
            total_cache_read_tokens: 30,
            total_cache_creation_tokens: 40,
        };
        let restored_child = PreparedForkRestore {
            history: child_state.history.clone(),
            recent_tools: child_state.recent_tools.clone(),
            activated_deferred_tool_names: Vec::new(),
            csl_manager: None,
            journal_state: child_state,
            last_turn_event: None,
        };

        apply_prepared_fork_restore(
            &mut state,
            "parent-session",
            "child-session",
            restored_child,
            None,
        )
        .await
        .unwrap();

        let finalized = crate::cli::session::session_input::finalize_effective_line(
            crate::cli::session::session_input::PreparedInput::user_only("continue"),
            "continue".into(),
            None,
            &mut state,
        )
        .await;

        assert!(
            finalized.runtime_volatile_texts.is_empty(),
            "new forked child should not immediately inherit parent reminder pressure: {finalized:?}"
        );
        assert_eq!(finalized.user_message, "continue");
        assert_eq!(state.turns_since_task_use, 1);
        assert_eq!(state.turns_since_task_reminder, 1);
    }

    #[tokio::test]
    async fn apply_prepared_fork_restore_preserves_existing_child_task_board() {
        let mut state = SessionState::default();
        state.set_session_id("parent-session");
        let parent_created = state
            .task_manager
            .create(&serde_json::json!({"title": "parent open work"}))
            .await;
        assert!(parent_created.contains("created"), "{parent_created}");

        let child_manager = astra_tools::task_mgmt::TaskManager::new(
            "child-session".to_string(),
            state.task_manager.store(),
        );
        let child_created = child_manager
            .create(&serde_json::json!({"title": "child existing work"}))
            .await;
        assert!(child_created.contains("created"), "{child_created}");

        let child_state = session_runtime::RestoredSessionState {
            history: vec![("child-q".into(), "child-a".into())],
            turn: 1,
            recent_tools: vec!["task_board".into()],
            total_prompt_tokens: 10,
            total_completion_tokens: 20,
            total_cache_read_tokens: 30,
            total_cache_creation_tokens: 40,
        };
        let restored_child = PreparedForkRestore {
            history: child_state.history.clone(),
            recent_tools: child_state.recent_tools.clone(),
            activated_deferred_tool_names: Vec::new(),
            csl_manager: None,
            journal_state: child_state,
            last_turn_event: None,
        };

        let outcome = apply_prepared_fork_restore(
            &mut state,
            "parent-session",
            "child-session",
            restored_child,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ForkTaskBoardRestore::PreservedExistingChild);

        let child_list = state
            .task_manager
            .list(&serde_json::json!({"status_filter": "all"}))
            .await;
        assert!(
            child_list.contains("child existing work"),
            "fork restore must not overwrite an existing child task board: {child_list}"
        );
        assert!(
            !child_list.contains("parent open work"),
            "parent task board should only be copied into an empty child session: {child_list}"
        );
    }

    #[tokio::test]
    async fn apply_prepared_fork_restore_fails_before_switching_when_task_load_fails() {
        let mut state = SessionState::default();
        state.task_manager = std::sync::Arc::new(astra_tools::task_mgmt::TaskManager::new(
            "parent-session",
            std::sync::Arc::new(FailingLoadTaskStore),
        ));
        state.set_session_id("parent-session");
        state.turn = 7;
        state.history = vec![("parent-q".into(), "parent-a".into())];

        let child_state = session_runtime::RestoredSessionState {
            history: vec![("child-q".into(), "child-a".into())],
            turn: 1,
            recent_tools: vec!["task_board".into()],
            total_prompt_tokens: 10,
            total_completion_tokens: 20,
            total_cache_read_tokens: 30,
            total_cache_creation_tokens: 40,
        };
        let restored_child = PreparedForkRestore {
            history: child_state.history.clone(),
            recent_tools: child_state.recent_tools.clone(),
            activated_deferred_tool_names: Vec::new(),
            csl_manager: None,
            journal_state: child_state,
            last_turn_event: None,
        };

        let error = apply_prepared_fork_restore(
            &mut state,
            "parent-session",
            "child-session",
            restored_child,
            None,
        )
        .await
        .expect_err("task load failure should abort fork restore");

        assert!(
            error.contains("load existing task board for forked child child-session"),
            "{error}"
        );
        assert_eq!(state.session_id.as_deref(), Some("parent-session"));
        assert_eq!(state.turn, 7);
        assert_eq!(
            state.history,
            vec![("parent-q".to_string(), "parent-a".to_string())]
        );
    }

    #[tokio::test]
    async fn apply_prepared_fork_restore_requires_cloud_copy_client_for_cloud_task_board() {
        let mut state = SessionState::default();
        state.set_session_id("parent-session");
        state.turn = 7;
        let (notify_tx, _) = tokio::sync::broadcast::channel(1);
        state.task_notify_tx = Some(notify_tx);
        let created = state
            .task_manager
            .create(&serde_json::json!({"title": "cloud-owned task"}))
            .await;
        assert!(created.contains("created"), "{created}");

        let child_state = session_runtime::RestoredSessionState {
            history: vec![("child-q".into(), "child-a".into())],
            turn: 1,
            recent_tools: vec!["task_board".into()],
            total_prompt_tokens: 10,
            total_completion_tokens: 20,
            total_cache_read_tokens: 30,
            total_cache_creation_tokens: 40,
        };
        let restored_child = PreparedForkRestore {
            history: child_state.history.clone(),
            recent_tools: child_state.recent_tools.clone(),
            activated_deferred_tool_names: Vec::new(),
            csl_manager: None,
            journal_state: child_state,
            last_turn_event: None,
        };

        let error = apply_prepared_fork_restore(
            &mut state,
            "parent-session",
            "child-session",
            restored_child,
            None,
        )
        .await
        .expect_err("cloud fork must fail closed when copy client is missing");
        assert!(
            error.contains("cloud task board fork copy is unavailable"),
            "{error}"
        );
        assert_eq!(state.session_id.as_deref(), Some("parent-session"));
        assert_eq!(state.turn, 7);
    }
}
