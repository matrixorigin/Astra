#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionSourceKind {
    WorkspaceInvalid,
    Cloud,
    Local,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionSourceSurface<'a> {
    raw: &'a str,
    kind: SessionSourceKind,
}

pub(crate) fn session_source_kind(
    last_status: &str,
    restored_from_cloud: bool,
    workspace_error: bool,
) -> SessionSourceKind {
    if workspace_error {
        SessionSourceKind::WorkspaceInvalid
    } else if restored_from_cloud {
        SessionSourceKind::Cloud
    } else if last_status == "local" {
        SessionSourceKind::Local
    } else {
        SessionSourceKind::Other
    }
}

pub(crate) fn session_source_surface(
    last_status: &str,
    restored_from_cloud: bool,
    workspace_error: bool,
) -> SessionSourceSurface<'_> {
    SessionSourceSurface {
        raw: last_status,
        kind: session_source_kind(last_status, restored_from_cloud, workspace_error),
    }
}

impl SessionSourceSurface<'_> {
    pub(crate) fn badge(&self) -> &str {
        match self.kind {
            SessionSourceKind::WorkspaceInvalid => "!",
            SessionSourceKind::Cloud => "☁",
            SessionSourceKind::Local => "⊙",
            SessionSourceKind::Other => self.raw,
        }
    }

    pub(crate) fn label(&self) -> &str {
        match self.kind {
            SessionSourceKind::WorkspaceInvalid => "metadata-warning",
            SessionSourceKind::Cloud => "cloud",
            SessionSourceKind::Local => "local",
            SessionSourceKind::Other => self.raw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_source_surface_prioritizes_workspace_error_and_cloud() {
        let invalid = session_source_surface("local", false, true);
        assert_eq!(invalid.badge(), "!");
        assert_eq!(invalid.label(), "metadata-warning");

        let cloud = session_source_surface("local", true, false);
        assert_eq!(cloud.badge(), "☁");
        assert_eq!(cloud.label(), "cloud");
    }

    #[test]
    fn session_source_surface_maps_local_and_preserves_other_labels() {
        let local = session_source_surface("local", false, false);
        assert_eq!(local.badge(), "⊙");
        assert_eq!(local.label(), "local");

        let active = session_source_surface("active", false, false);
        assert_eq!(active.badge(), "active");
        assert_eq!(active.label(), "active");
    }
}
