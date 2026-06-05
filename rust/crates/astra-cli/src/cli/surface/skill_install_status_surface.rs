use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillInstallStatusKind {
    Installed,
    Upgraded,
    RolledBack,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SkillInstallStatusSurface<'a> {
    raw: &'a str,
    kind: SkillInstallStatusKind,
}

pub(crate) fn skill_install_status_kind(status: &str) -> SkillInstallStatusKind {
    match status {
        "installed" => SkillInstallStatusKind::Installed,
        "upgraded" => SkillInstallStatusKind::Upgraded,
        "rolled_back" => SkillInstallStatusKind::RolledBack,
        _ => SkillInstallStatusKind::Other,
    }
}

pub(crate) fn skill_install_status_surface(status: &str) -> SkillInstallStatusSurface<'_> {
    SkillInstallStatusSurface {
        raw: status,
        kind: skill_install_status_kind(status),
    }
}

impl SkillInstallStatusSurface<'_> {
    pub(crate) fn label(&self) -> &str {
        self.raw
    }

    pub(crate) fn styled_label(self) -> String {
        match self.kind {
            SkillInstallStatusKind::Installed => self.raw.green().to_string(),
            SkillInstallStatusKind::Upgraded => self.raw.magenta().to_string(),
            SkillInstallStatusKind::RolledBack => self.raw.yellow().to_string(),
            SkillInstallStatusKind::Other => self.raw.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_install_status_helpers_classify_known_statuses() {
        assert_eq!(
            skill_install_status_kind("installed"),
            SkillInstallStatusKind::Installed
        );
        assert_eq!(
            skill_install_status_kind("upgraded"),
            SkillInstallStatusKind::Upgraded
        );
        assert_eq!(
            skill_install_status_kind("rolled_back"),
            SkillInstallStatusKind::RolledBack
        );
        assert_eq!(
            skill_install_status_kind("pending"),
            SkillInstallStatusKind::Other
        );
    }

    #[test]
    fn skill_install_status_surface_styles_known_statuses_and_preserves_unknown() {
        let installed = skill_install_status_surface("installed");
        let upgraded = skill_install_status_surface("upgraded");
        let rolled_back = skill_install_status_surface("rolled_back");
        let other = skill_install_status_surface("pending");

        assert_eq!(installed.label(), "installed");
        assert_eq!(
            crate::cli::theme::strip_ansi(&installed.styled_label()),
            "installed"
        );
        assert_eq!(
            crate::cli::theme::strip_ansi(&upgraded.styled_label()),
            "upgraded"
        );
        assert_eq!(
            crate::cli::theme::strip_ansi(&rolled_back.styled_label()),
            "rolled_back"
        );
        assert_eq!(other.styled_label(), "pending");
    }
}
