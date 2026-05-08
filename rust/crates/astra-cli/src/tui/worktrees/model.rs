//! Pure worktree-list model — RED phase stub.

#![allow(dead_code)]

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeEntry {
    pub path: String,
    /// Branch name without the `refs/heads/` prefix. `None` for
    /// detached HEAD or bare.
    pub branch: Option<String>,
    /// Short SHA (7-char) if available.
    pub head: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    /// Number of astra sessions recorded in this worktree.
    pub session_count: usize,
    /// ISO 8601 timestamp of the most recent session's last update.
    pub last_session_at: Option<String>,
}

impl WorktreeEntry {
    /// Human label combining branch + head for the list column.
    pub fn label(&self) -> String {
        match (&self.branch, &self.head) {
            (Some(b), Some(h)) => format!("⎇ {b} @ {h}"),
            (Some(b), None) => format!("⎇ {b}"),
            (None, Some(h)) if self.is_detached => format!("(detached @ {h})"),
            (None, Some(h)) => format!("@ {h}"),
            (None, None) if self.is_bare => "(bare)".to_string(),
            (None, None) => "(unknown)".to_string(),
        }
    }
}

/// Parse `git worktree list --porcelain` output into structured entries.
///
/// The porcelain format is record-based; records separated by blank
/// lines. Each record starts with `worktree <path>`, optionally
/// followed by `HEAD <sha>`, `branch refs/heads/<name>`, `bare`, or
/// `detached`.  Unknown fields are ignored.
pub(crate) fn parse(porcelain: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;

    for raw_line in porcelain.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            cur = Some(WorktreeEntry {
                path: path.to_string(),
                branch: None,
                head: None,
                is_bare: false,
                is_detached: false,
                session_count: 0,
                last_session_at: None,
            });
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(e) = cur.as_mut() {
                let short: String = head.chars().take(7).collect();
                e.head = Some(short);
            }
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(e) = cur.as_mut() {
                e.branch = Some(branch.to_string());
            }
        } else if line == "bare" {
            if let Some(e) = cur.as_mut() {
                e.is_bare = true;
            }
        } else if line == "detached" {
            if let Some(e) = cur.as_mut() {
                e.is_detached = true;
            }
        }
        // Unknown lines are silently skipped (best-effort parsing).
    }

    if let Some(e) = cur.take() {
        out.push(e);
    }
    out
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorktreeList {
    entries: Vec<WorktreeEntry>,
    selected: usize,
}

impl WorktreeList {
    pub fn new(entries: Vec<WorktreeEntry>) -> Self {
        Self { entries, selected: 0 }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[WorktreeEntry] {
        &self.entries
    }

    pub fn selected(&self) -> Option<usize> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    pub fn selected_entry(&self) -> Option<&WorktreeEntry> {
        self.entries.get(self.selected)
    }

    pub fn move_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.entries.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.entries.len();
    }
}
