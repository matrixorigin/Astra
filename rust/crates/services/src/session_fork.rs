//! Fork a local session journal + workspace for experimentation, multi-agent branches, or cloud sync lineage.
//!
//! Produces a new session id, writes `session_fork` + `session_start` + copied events (excluding the
//! parent's `session_start` / `session_end`), and a new `workspace.yaml` with parent linkage.

use crate::session_journal::{
    JournalEvent, JournalEventType, JournalWriter, SessionLineage, journal_file_path, read_journal,
};
use crate::session_workspace::{self, WorkspaceMetadata};

/// Options for [`fork_local_session`].
#[derive(Debug, Clone)]
pub struct ForkSessionOptions {
    pub parent_session_id: String,
    /// When `None`, a new UUID v4 is generated.
    pub new_session_id: Option<String>,
    pub label: Option<String>,
}

/// Result of a successful fork.
#[derive(Debug, Clone)]
pub struct ForkSessionResult {
    pub new_session_id: String,
    /// Turn-like events copied from parent (excludes synthetic fork/start lines).
    pub events_copied: usize,
}

/// Fork parent journal into a new session file and workspace metadata.
///
/// Fails if the target journal path already exists or the parent journal is empty.
pub fn fork_local_session(opts: ForkSessionOptions) -> Result<ForkSessionResult, String> {
    let parent = opts.parent_session_id.trim().to_string();
    if parent.is_empty() {
        return Err("parent_session_id is empty".into());
    }

    let new_id = opts
        .new_session_id
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let dest = journal_file_path(&new_id);
    if dest.exists() {
        return Err(format!(
            "refusing to fork: journal file already exists for session {new_id}"
        ));
    }

    let events = read_journal(&parent).map_err(|e| e.to_string())?;
    if events.is_empty() {
        return Err(format!("parent session {parent} has no journal events"));
    }

    let model = events
        .iter()
        .find_map(|e| {
            (e.event_type == JournalEventType::SessionStart)
                .then_some(e.model.clone())
                .flatten()
        })
        .or_else(|| events.iter().find_map(|e| e.model.clone()));

    let forked_at_turn = session_workspace::read_workspace(&parent)
        .map(|w| w.turn_count)
        .unwrap_or_else(|_| {
            events
                .iter()
                .filter(|e| e.event_type == JournalEventType::Turn)
                .count() as u32
        });

    let lineage = SessionLineage {
        parent_session_id: parent.clone(),
        forked_after_turn: Some(forked_at_turn),
        label: opts.label.clone(),
    };

    let fork_evt = JournalEvent::session_fork(
        Some(new_id.as_str()),
        lineage.clone(),
        opts.label.as_deref(),
    );

    let mut start = JournalEvent::session_start(Some(new_id.as_str()), model.as_deref());
    start.session_lineage = Some(lineage);

    let mut out: Vec<JournalEvent> = vec![fork_evt, start];
    let mut copied = 0usize;

    for mut evt in events {
        if matches!(
            evt.event_type,
            JournalEventType::SessionStart | JournalEventType::SessionEnd
        ) {
            continue;
        }
        evt.session_id = Some(new_id.clone());
        out.push(evt);
        copied += 1;
    }

    let writer = JournalWriter::new(&new_id).map_err(|e| e.to_string())?;
    for evt in &out {
        writer.append(evt).map_err(|e| e.to_string())?;
    }

    let mut ws = session_workspace::read_workspace(&parent)
        .unwrap_or_else(|_| WorkspaceMetadata::new(&parent, model.as_deref().unwrap_or("default")));
    ws.session_id = new_id.clone();
    ws.parent_session_id = Some(parent.clone());
    ws.fork_note = opts.label.clone();
    ws.forked_at_turn = Some(forked_at_turn);
    // Carry forward an existing correlation id, else use parent session id as chain root for multi-agent / audit.
    ws.correlation_id = session_workspace::read_workspace(&parent)
        .ok()
        .and_then(|w| w.correlation_id.clone())
        .or_else(|| Some(parent.clone()));
    ws.agent_role = None;
    let now = chrono::Utc::now().to_rfc3339();
    ws.created_at = now.clone();
    ws.updated_at = now;
    ws.status = "active".to_string();
    session_workspace::write_workspace(&ws).map_err(|e| e.to_string())?;

    Ok(ForkSessionResult {
        new_session_id: new_id,
        events_copied: copied,
    })
}
