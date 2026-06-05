//! Pure `SessionDiscovery`.

#![allow(dead_code)]

use std::sync::Arc;

use nucleo_matcher::{pattern::Atom, Config, Matcher, Utf32Str};

use crate::tui::{transcript_jsonl, turn_event::TurnEvent};

/// A single resumable session surfaced to the picker.
///
/// `updated_at_age_secs` is precomputed age-in-seconds at enumeration
/// time so rendering doesn't need a `SystemTime::now()` call (keeps
/// snapshot tests deterministic).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionEntry {
    pub id: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub turn_count: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Session-level cumulative USD cost (sum across turns). `None`
    /// when the workspace predates cost tracking.
    pub cost_usd: Option<f64>,
    pub summary: Option<String>,
    pub status: String,
    pub model: String,
    pub updated_at: String,
    pub checkpoints: u32,
    pub plan_goal: Option<String>,
}

/// IO abstraction for loading session metadata.
///
/// Production impl queries `astra_services::session_workspace`; tests
/// use the in-memory [`StaticSessionSource`].
pub(crate) trait SessionSource: std::fmt::Debug + Send + Sync {
    fn list(&self, limit: usize) -> Vec<SessionEntry>;
}

// ── Filesystem-backed source (production) ─────────────────────────

/// Reads sessions from `astra_services::session_workspace`. Returns
/// most-recent first.
#[derive(Debug, Default, Clone)]
pub(crate) struct FsSessionSource;

impl FsSessionSource {
    pub fn new() -> Self {
        Self
    }
}

impl SessionSource for FsSessionSource {
    fn list(&self, limit: usize) -> Vec<SessionEntry> {
        use astra_services::{session_journal, session_workspace};
        let ids = match session_journal::list_sessions_by_time(limit) {
            Ok(ids) => ids,
            Err(_) => return Vec::new(),
        };
        ids.into_iter()
            .filter_map(|sid| {
                let peek = session_journal::peek_session_meta(&sid);
                let workspace = match session_workspace::read_workspace_optional(&sid) {
                    Ok(workspace) => workspace,
                    Err(error) => {
                        tracing::warn!(
                            "session picker failed to read workspace for {}: {}",
                            sid,
                            error
                        );
                        None
                    }
                };
                let cost_usd = transcript_cost_usd(&sid);
                workspace
                    .map(|ws| SessionEntry {
                        id: sid.clone(),
                        cwd: ws.cwd,
                        git_branch: ws.git_branch,
                        git_head: ws.git_head,
                        turn_count: ws.turn_count,
                        tokens_in: ws.total_tokens_in,
                        tokens_out: ws.total_tokens_out,
                        cost_usd,
                        summary: decorate_picker_summary(
                            ws.summary,
                            ws.last_persistence_error.as_deref(),
                        ),
                        status: ws.status,
                        model: ws.model.unwrap_or_else(|| "default".to_string()),
                        updated_at: ws.updated_at,
                        checkpoints: ws.checkpoints.len() as u32,
                        plan_goal: ws.plan_goal,
                    })
                    .or_else(|| {
                        peek.map(|peek| SessionEntry {
                            id: sid.clone(),
                            cwd: "(workspace unavailable)".to_string(),
                            git_branch: None,
                            git_head: None,
                            turn_count: session_journal::count_turns(&sid),
                            tokens_in: 0,
                            tokens_out: 0,
                            cost_usd,
                            summary: peek.first_prompt,
                            status: "journal_only".to_string(),
                            model: peek.model.unwrap_or_else(|| "default".to_string()),
                            updated_at: peek.created_at.unwrap_or_default(),
                            checkpoints: 0,
                            plan_goal: None,
                        })
                    })
            })
            .collect()
    }
}

fn decorate_picker_summary(
    summary: Option<String>,
    persistence_error: Option<&str>,
) -> Option<String> {
    let persistence_error = persistence_error
        .map(str::trim)
        .filter(|error| !error.is_empty());
    match (summary, persistence_error) {
        (Some(summary), Some(error)) => Some(format!(
            "persistence degraded: {} · {}",
            summarize_picker_error(error),
            summary
        )),
        (None, Some(error)) => Some(format!(
            "persistence degraded: {}",
            summarize_picker_error(error)
        )),
        (summary, None) => summary,
    }
}

fn summarize_picker_error(error: &str) -> String {
    const LIMIT: usize = 56;
    let mut chars = error.chars();
    let preview: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn transcript_cost_usd(session_id: &str) -> Option<f64> {
    transcript_jsonl::load(session_id)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            TurnEvent::TurnSummary {
                cumulative_cost_usd,
                ..
            } => cumulative_cost_usd,
            _ => None,
        })
}

// ── Static source for tests ───────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct StaticSessionSource {
    pub entries: Vec<SessionEntry>,
}

impl StaticSessionSource {
    pub fn new(entries: Vec<SessionEntry>) -> Self {
        Self { entries }
    }
}

impl SessionSource for StaticSessionSource {
    fn list(&self, limit: usize) -> Vec<SessionEntry> {
        self.entries.iter().take(limit).cloned().collect()
    }
}

// ── Discovery engine — RED stubs ──────────────────────────────────

pub(crate) struct SessionDiscovery {
    source: Arc<dyn SessionSource>,
    entries: Vec<SessionEntry>,
    /// Indices of `entries` matching the current filter, ranked
    /// best-first by fuzzy score.
    filtered: Vec<usize>,
    selected: usize,
    filter: String,
}

impl std::fmt::Debug for SessionDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionDiscovery")
            .field("entries_len", &self.entries.len())
            .field("filtered_len", &self.filtered.len())
            .field("selected", &self.selected)
            .field("filter", &self.filter)
            .finish()
    }
}

impl SessionDiscovery {
    pub fn new<S: SessionSource + 'static>(source: S, limit: usize) -> Self {
        Self::from_arc(Arc::new(source), limit)
    }

    pub fn from_arc(source: Arc<dyn SessionSource>, limit: usize) -> Self {
        let entries = source.list(limit);
        let filtered: Vec<usize> = (0..entries.len()).collect();
        Self {
            source,
            entries,
            filtered,
            selected: 0,
            filter: String::new(),
        }
    }

    pub fn total(&self) -> usize {
        self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.filtered.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Update the fuzzy filter. Matches against a haystack built from
    /// `{id} {cwd} {branch} {summary}`.
    pub fn set_filter(&mut self, query: &str) {
        self.filter = query.to_string();

        if query.is_empty() {
            self.filtered = (0..self.entries.len()).collect();
            self.clamp_selected();
            return;
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let atom = Atom::new(
            query,
            nucleo_matcher::pattern::CaseMatching::Ignore,
            nucleo_matcher::pattern::Normalization::Smart,
            nucleo_matcher::pattern::AtomKind::Fuzzy,
            false,
        );

        let mut scored: Vec<(u16, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let hay = haystack(e);
                let mut buf = Vec::new();
                let hay_utf32 = Utf32Str::new(&hay, &mut buf);
                atom.score(hay_utf32, &mut matcher).map(|s| (s, i))
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        self.clamp_selected();
    }

    /// Entries visible given the current filter, ranked best-first.
    pub fn matches(&self) -> Vec<&SessionEntry> {
        self.filtered.iter().map(|&i| &self.entries[i]).collect()
    }

    pub fn selected(&self) -> Option<usize> {
        if self.filtered.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    pub fn selected_entry(&self) -> Option<&SessionEntry> {
        self.filtered.get(self.selected).map(|&i| &self.entries[i])
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.filtered.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.filtered.len();
    }

    /// Consume the selection as a resume target (session id).
    pub fn accept(&self) -> Option<String> {
        self.selected_entry().map(|e| e.id.clone())
    }

    fn clamp_selected(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }
}

/// Build a fuzzy-matchable haystack from a session entry.
fn haystack(e: &SessionEntry) -> String {
    let mut s = String::new();
    s.push_str(&e.id);
    s.push(' ');
    s.push_str(&e.cwd);
    if let Some(b) = e.git_branch.as_deref() {
        s.push(' ');
        s.push_str(b);
    }
    if let Some(sum) = e.summary.as_deref() {
        s.push(' ');
        s.push_str(sum);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{
        session_journal::{JournalDirGuard, JournalEvent, JournalWriter},
        session_workspace,
    };

    fn with_tmp_sessions_dir<F: FnOnce(&std::path::Path)>(f: F) {
        let cwd = std::env::current_dir().expect("current dir");
        let tmp = tempfile::Builder::new()
            .prefix("session-picker-discovery-")
            .tempdir_in(&cwd)
            .expect("tempdir in cwd");
        f(tmp.path());
    }

    fn write_picker_session(
        sessions_dir: &std::path::Path,
        session_id: &str,
    ) -> session_workspace::WorkspaceMetadata {
        std::fs::create_dir_all(sessions_dir).expect("create sessions dir");
        std::fs::write(sessions_dir.join(format!("{session_id}.jsonl")), "").expect("journal file");
        let mut ws = session_workspace::WorkspaceMetadata::new(session_id, "claude-sonnet-4.6");
        ws.cwd = "/tmp/astra".into();
        ws.updated_at = "2026-05-18T12:00:00Z".into();
        ws.turn_count = 3;
        ws.total_tokens_in = 1_000;
        ws.total_tokens_out = 500;
        ws.summary = Some("rich metadata".into());
        session_workspace::write_workspace(&ws).expect("workspace yaml");
        ws
    }

    #[test]
    #[serial_test::serial]
    fn fs_source_reads_cost_from_latest_transcript_turn_summary() {
        with_tmp_sessions_dir(|sessions_dir| {
            let _guard = JournalDirGuard::new(sessions_dir);
            let sid = "sessmeta123";
            let _ws = write_picker_session(&sessions_dir, sid);

            transcript_jsonl::append(
                sid,
                &TurnEvent::TurnSummary {
                    ts: None,
                    elapsed_ms: Some(1200),
                    ttft_ms: None,
                    tokens_in: Some(300),
                    tokens_out: Some(120),
                    cache_read_tokens: None,
                    tools: 1,
                    cumulative_tokens: Some(420),
                    cumulative_cost_usd: Some(1.23),
                },
            );

            let entries = FsSessionSource::new().list(10);
            let entry = entries
                .iter()
                .find(|entry| entry.id == sid)
                .expect("session listed");
            assert_eq!(entry.cost_usd, Some(1.23));
        });
    }

    #[test]
    #[serial_test::serial]
    fn fs_source_uses_last_non_empty_transcript_cost() {
        with_tmp_sessions_dir(|sessions_dir| {
            let _guard = JournalDirGuard::new(sessions_dir);
            let sid = "sessmeta456";
            let _ws = write_picker_session(&sessions_dir, sid);

            transcript_jsonl::append(
                sid,
                &TurnEvent::TurnSummary {
                    ts: None,
                    elapsed_ms: Some(1000),
                    ttft_ms: None,
                    tokens_in: Some(200),
                    tokens_out: Some(100),
                    cache_read_tokens: None,
                    tools: 0,
                    cumulative_tokens: Some(300),
                    cumulative_cost_usd: Some(0.42),
                },
            );
            transcript_jsonl::append(
                sid,
                &TurnEvent::TurnSummary {
                    ts: None,
                    elapsed_ms: Some(1200),
                    ttft_ms: None,
                    tokens_in: Some(250),
                    tokens_out: Some(120),
                    cache_read_tokens: None,
                    tools: 1,
                    cumulative_tokens: Some(670),
                    cumulative_cost_usd: None,
                },
            );

            let entries = FsSessionSource::new().list(10);
            let entry = entries
                .iter()
                .find(|entry| entry.id == sid)
                .expect("session listed");
            assert_eq!(entry.cost_usd, Some(0.42));
        });
    }

    #[test]
    #[serial_test::serial]
    fn fs_source_keeps_session_when_workspace_is_corrupt() {
        with_tmp_sessions_dir(|sessions_dir| {
            let _guard = JournalDirGuard::new(sessions_dir);
            let sid = "sessmeta789";
            JournalWriter::new(sid)
                .unwrap()
                .append(&JournalEvent::session_start(Some(sid), Some("gpt-5")))
                .unwrap();
            JournalWriter::new(sid)
                .unwrap()
                .append(&JournalEvent::turn(
                    Some(sid),
                    1,
                    Some("gpt-5"),
                    "resume me",
                    "done",
                    0,
                    10,
                    20,
                    30,
                ))
                .unwrap();
            let workspace_path = session_workspace::workspace_file_path(sid).unwrap();
            std::fs::create_dir_all(workspace_path.parent().unwrap()).unwrap();
            std::fs::write(&workspace_path, ":\nnot-valid-yaml").unwrap();

            let entries = FsSessionSource::new().list(10);
            let entry = entries
                .iter()
                .find(|entry| entry.id == sid)
                .expect("corrupt-workspace session still listed");
            assert_eq!(entry.status, "journal_only");
            assert_eq!(entry.summary.as_deref(), Some("resume me"));
            assert_eq!(entry.turn_count, 1);
            assert_eq!(entry.cwd, "(workspace unavailable)");
        });
    }

    #[test]
    #[serial_test::serial]
    fn fs_source_surfaces_persistence_degradation_in_summary() {
        with_tmp_sessions_dir(|sessions_dir| {
            let _guard = JournalDirGuard::new(sessions_dir);
            let sid = "sessmeta-degraded";
            let mut ws = write_picker_session(sessions_dir, sid);
            ws.last_persistence_error = Some("failed to append turn event".into());
            session_workspace::write_workspace(&ws).expect("rewrite workspace yaml");

            let entries = FsSessionSource::new().list(10);
            let entry = entries
                .iter()
                .find(|entry| entry.id == sid)
                .expect("session listed");
            let summary = entry.summary.as_deref().expect("summary should be present");
            assert!(summary.contains("persistence degraded"), "{summary}");
            assert!(summary.contains("rich metadata"), "{summary}");
        });
    }
}
