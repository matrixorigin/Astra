//! Pure `SessionDiscovery`.

#![allow(dead_code)]

use std::sync::Arc;

use nucleo_matcher::{Config, Matcher, Utf32Str, pattern::Atom};

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
                let ws = session_workspace::read_workspace(&sid).ok()?;
                Some(SessionEntry {
                    id: sid,
                    cwd: ws.cwd,
                    git_branch: ws.git_branch,
                    git_head: ws.git_head,
                    turn_count: ws.turn_count,
                    tokens_in: ws.total_tokens_in,
                    tokens_out: ws.total_tokens_out,
                    // Workspace metadata doesn't currently persist
                    // session cost — leave `None` for now; the widget
                    // hides the column when the whole list is `None`.
                    cost_usd: None,
                    summary: ws.summary,
                    status: ws.status,
                    model: ws.model.unwrap_or_else(|| "default".to_string()),
                    updated_at: ws.updated_at,
                    checkpoints: ws.checkpoints.len() as u32,
                    plan_goal: ws.plan_goal,
                })
            })
            .collect()
    }
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
