//! Process-wide path policy for Astra-owned local files.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Optional process-local root for Astra-owned files.
///
/// External orchestrators use this instead of rewriting `HOME`.
pub const ASTRA_LOCAL_STATE_ROOT_ENV: &str = "ASTRA_LOCAL_STATE_ROOT";

/// Return the explicitly configured Astra-local root.
///
/// Empty values are treated as absent.
#[must_use]
pub fn local_state_root_override() -> Option<PathBuf> {
    local_state_root_override_from(std::env::var_os(ASTRA_LOCAL_STATE_ROOT_ENV).as_deref())
}

/// Resolve the root for Astra-owned local files.
///
/// Callers choose typed child paths such as `sessions`, `config`, or
/// `permissions.json`; this function owns the common root policy.
#[must_use]
pub fn local_state_root() -> PathBuf {
    local_state_root_from(
        local_state_root_override().as_deref(),
        dirs::home_dir().as_deref(),
    )
}

fn local_state_root_override_from(value: Option<&OsStr>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn local_state_root_from(explicit: Option<&Path>, home: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.unwrap_or_else(|| Path::new(".")).join(".astra"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_root_is_authoritative_without_rewriting_home() {
        let root = PathBuf::from("/isolated/astra");
        let home = PathBuf::from("/developer/home");

        assert_eq!(
            local_state_root_override_from(Some(root.as_os_str())),
            Some(root.clone())
        );
        assert_eq!(local_state_root_from(Some(&root), Some(&home)), root);
    }

    #[test]
    fn empty_override_is_absent() {
        assert_eq!(local_state_root_override_from(Some(OsStr::new(""))), None);
        assert_eq!(
            local_state_root_from(None, Some(Path::new("/developer/home"))),
            PathBuf::from("/developer/home/.astra")
        );
    }
}
