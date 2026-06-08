#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionWorkspaceStatusKind {
    Active,
    Completed,
    Error,
    JournalOnly,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionWorkspaceStatusSurface<'a> {
    raw: &'a str,
    kind: SessionWorkspaceStatusKind,
}

pub(crate) fn session_workspace_status_kind(status: &str) -> SessionWorkspaceStatusKind {
    match status {
        "active" => SessionWorkspaceStatusKind::Active,
        "completed" => SessionWorkspaceStatusKind::Completed,
        "error" => SessionWorkspaceStatusKind::Error,
        "journal_only" => SessionWorkspaceStatusKind::JournalOnly,
        _ => SessionWorkspaceStatusKind::Other,
    }
}

pub(crate) fn session_workspace_status_surface(status: &str) -> SessionWorkspaceStatusSurface<'_> {
    SessionWorkspaceStatusSurface {
        raw: status,
        kind: session_workspace_status_kind(status),
    }
}

impl SessionWorkspaceStatusSurface<'_> {
    pub(crate) fn is_active(self) -> bool {
        self.kind == SessionWorkspaceStatusKind::Active
    }

    pub(crate) fn is_completed(self) -> bool {
        self.kind == SessionWorkspaceStatusKind::Completed
    }

    pub(crate) fn label(&self) -> &str {
        self.raw
    }

    pub(crate) fn icon(self) -> &'static str {
        match self.kind {
            SessionWorkspaceStatusKind::Active => "🔄",
            SessionWorkspaceStatusKind::Completed => "✅",
            SessionWorkspaceStatusKind::Error => "❌",
            SessionWorkspaceStatusKind::JournalOnly => "📓",
            SessionWorkspaceStatusKind::Other => "•",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        session_workspace_status_kind, session_workspace_status_surface, SessionWorkspaceStatusKind,
    };

    #[test]
    fn workspace_status_helpers_classify_known_statuses() {
        assert_eq!(
            session_workspace_status_kind("active"),
            SessionWorkspaceStatusKind::Active
        );
        assert_eq!(
            session_workspace_status_kind("completed"),
            SessionWorkspaceStatusKind::Completed
        );
        assert_eq!(
            session_workspace_status_kind("error"),
            SessionWorkspaceStatusKind::Error
        );
        assert_eq!(
            session_workspace_status_kind("journal_only"),
            SessionWorkspaceStatusKind::JournalOnly
        );
        assert_eq!(
            session_workspace_status_kind("paused"),
            SessionWorkspaceStatusKind::Other
        );
    }

    #[test]
    fn workspace_status_surface_exposes_icons_and_flags() {
        let active = session_workspace_status_surface("active");
        assert!(active.is_active());
        assert_eq!(active.icon(), "🔄");

        let completed = session_workspace_status_surface("completed");
        assert!(completed.is_completed());
        assert_eq!(completed.icon(), "✅");

        let other = session_workspace_status_surface("paused");
        assert!(!other.is_active());
        assert!(!other.is_completed());
        assert_eq!(other.icon(), "•");
        assert_eq!(other.label(), "paused");

        let journal_only = session_workspace_status_surface("journal_only");
        assert_eq!(journal_only.icon(), "📓");
        assert_eq!(journal_only.label(), "journal_only");
    }
}
