//! Path validation and boundary enforcement.

use super::policy::{SandboxMode, SandboxPolicy};
use std::path::{Path, PathBuf};

/// Error type for path validation failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPathError {
    /// Path escapes the allowed boundary after canonicalization.
    BoundaryEscape {
        requested: String,
        resolved: String,
        project_root: String,
    },
    /// Path contains a symlink that escapes the boundary.
    SymlinkEscape { requested: String, target: String },
    /// Path could not be resolved (doesn't exist, permission denied, etc.).
    ResolutionFailed { requested: String, reason: String },
}

impl std::fmt::Display for SandboxPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BoundaryEscape {
                requested,
                project_root,
                ..
            } => {
                write!(
                    f,
                    "Path '{requested}' escapes project boundary '{project_root}'"
                )
            }
            Self::SymlinkEscape { requested, target } => {
                write!(
                    f,
                    "Symlink '{requested}' points to '{target}' outside boundary"
                )
            }
            Self::ResolutionFailed { requested, reason } => {
                write!(f, "Cannot resolve path '{requested}': {reason}")
            }
        }
    }
}

impl std::error::Error for SandboxPathError {}

impl SandboxPathError {
    /// Returns `true` when the error is a boundary violation (not a resolution failure).
    /// Callers can use this to distinguish "needs user authorization" from "path is broken".
    pub fn is_boundary_violation(&self) -> bool {
        matches!(
            self,
            Self::BoundaryEscape { .. } | Self::SymlinkEscape { .. }
        )
    }
}

/// Validate and resolve a path against the sandbox policy.
///
/// For Permissive mode, returns the path as-is (backward compatible).
/// For Standard/Strict modes:
/// 1. Resolves the path (relative to project_root or absolute)
/// 2. Canonicalizes to resolve symlinks and `..` components
/// 3. Checks the canonical path is within allowed boundaries
///
/// # Errors
///
/// Returns `SandboxPathError` if the path escapes the boundary or can't be resolved.
pub fn validate_path(policy: &SandboxPolicy, path: &str) -> Result<PathBuf, SandboxPathError> {
    // Permissive mode: no validation, backward compatible
    if policy.mode == SandboxMode::Permissive {
        let p = Path::new(path);
        return Ok(if p.is_absolute() {
            p.to_path_buf()
        } else {
            policy.project_root.join(p)
        });
    }

    // Resolve the raw path
    let raw = Path::new(path);
    let resolved = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        policy.project_root.join(raw)
    };

    // For existing paths: canonicalize to follow symlinks and resolve ..
    let canonical = if resolved.exists() {
        resolved
            .canonicalize()
            .map_err(|e| SandboxPathError::ResolutionFailed {
                requested: path.to_string(),
                reason: e.to_string(),
            })?
    } else {
        // For new files: normalize the path components manually
        normalize_path(&resolved)
    };

    // Check boundary
    if policy.is_path_allowed(&canonical) {
        Ok(canonical)
    } else {
        Err(SandboxPathError::BoundaryEscape {
            requested: path.to_string(),
            resolved: canonical.display().to_string(),
            project_root: policy.project_root.display().to_string(),
        })
    }
}

/// Normalize path without filesystem access (for non-existent files).
///
/// Resolves `.` and `..` components lexically. This doesn't follow symlinks
/// but prevents obvious directory traversal attacks.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // Don't pop past root
                if components.last().is_some_and(|c| {
                    !matches!(
                        c,
                        std::path::Component::RootDir | std::path::Component::Prefix(_)
                    )
                }) {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {
                // Skip `.`
            }
            other => {
                components.push(other);
            }
        }
    }

    components.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standard_policy(root: &str) -> SandboxPolicy {
        SandboxPolicy::for_project(root)
    }

    fn strict_policy(root: &str) -> SandboxPolicy {
        SandboxPolicy::strict(root)
    }

    // ── Permissive mode (backward compatible) ────────────────────────────

    #[test]
    fn permissive_allows_absolute_path() {
        let p = SandboxPolicy::permissive("/home/user/project");
        let result = validate_path(&p, "/etc/passwd");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn permissive_resolves_relative() {
        let p = SandboxPolicy::permissive("/home/user/project");
        let result = validate_path(&p, "src/main.rs");
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            PathBuf::from("/home/user/project/src/main.rs")
        );
    }

    // ── Standard mode (boundary enforcement) ─────────────────────────────

    #[test]
    fn standard_allows_relative_within_project() {
        let p = standard_policy("/tmp");
        let result = validate_path(&p, "subdir/file.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn standard_blocks_dotdot_escape() {
        let p = standard_policy("/tmp/project");
        let result = validate_path(&p, "../../../etc/passwd");
        assert!(result.is_err());
        match result.unwrap_err() {
            SandboxPathError::BoundaryEscape { requested, .. } => {
                assert!(requested.contains("etc/passwd"));
            }
            other => panic!("expected BoundaryEscape, got: {other:?}"),
        }
    }

    #[test]
    fn standard_blocks_absolute_outside_root() {
        let p = standard_policy("/home/user/project");
        let result = validate_path(&p, "/etc/shadow");
        assert!(result.is_err());
    }

    #[test]
    fn standard_allows_tmp() {
        let p = standard_policy("/home/user/project");
        let result = validate_path(&p, "/tmp/build-output.tar.gz");
        assert!(result.is_ok());
    }

    #[test]
    fn standard_allows_absolute_within_project() {
        let p = standard_policy("/tmp");
        let result = validate_path(&p, "/tmp/somefile");
        assert!(result.is_ok());
    }

    // ── Strict mode ──────────────────────────────────────────────────────

    #[test]
    fn strict_blocks_var_tmp() {
        let p = strict_policy("/home/user/project");
        let result = validate_path(&p, "/var/tmp/secret");
        assert!(result.is_err());
    }

    #[test]
    fn strict_allows_tmp() {
        let p = strict_policy("/home/user/project");
        let result = validate_path(&p, "/tmp/build.log");
        assert!(result.is_ok());
    }

    // ── Path normalization ───────────────────────────────────────────────

    #[test]
    fn normalize_removes_dotdot() {
        // /home/user/project + ../../etc/passwd → /home/etc/passwd
        // (two `..` pops `project` and `user`, leaves `/home`)
        let result = normalize_path(Path::new("/home/user/project/../../etc/passwd"));
        assert_eq!(result, PathBuf::from("/home/etc/passwd"));
    }

    #[test]
    fn normalize_removes_dot() {
        let result = normalize_path(Path::new("/home/user/./project/./src"));
        assert_eq!(result, PathBuf::from("/home/user/project/src"));
    }

    #[test]
    fn normalize_does_not_pop_past_root() {
        let result = normalize_path(Path::new("/../../../../../../etc"));
        assert_eq!(result, PathBuf::from("/etc"));
    }

    #[test]
    fn normalize_relative_path() {
        let result = normalize_path(Path::new("a/b/../c"));
        assert_eq!(result, PathBuf::from("a/c"));
    }

    // ── Real filesystem tests ────────────────────────────────────────────

    #[test]
    fn validates_existing_path_in_project() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        let p = standard_policy(dir.path().to_str().unwrap());
        let result = validate_path(&p, "test.txt");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), file.canonicalize().unwrap());
    }

    #[test]
    fn validates_new_file_in_project() {
        let dir = tempfile::tempdir().unwrap();
        let p = standard_policy(dir.path().to_str().unwrap());
        let result = validate_path(&p, "newfile.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn blocks_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        // Create a symlink that points outside project
        let link_path = dir.path().join("escape_link");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", &link_path).unwrap();
            let p = standard_policy(dir.path().to_str().unwrap());
            let result = validate_path(&p, "escape_link/passwd");
            // After canonicalization, this resolves to /etc/passwd → blocked
            assert!(result.is_err(), "symlink escape should be blocked");
        }
    }

    // ── Error display ────────────────────────────────────────────────────

    #[test]
    fn error_display_boundary() {
        let err = SandboxPathError::BoundaryEscape {
            requested: "../secret".into(),
            resolved: "/etc/secret".into(),
            project_root: "/home/user/project".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("escapes project boundary"));
        assert!(msg.contains("../secret"));
    }

    #[test]
    fn error_display_symlink() {
        let err = SandboxPathError::SymlinkEscape {
            requested: "link".into(),
            target: "/etc/passwd".into(),
        };
        assert!(err.to_string().contains("outside boundary"));
    }

    #[test]
    fn error_display_resolution() {
        let err = SandboxPathError::ResolutionFailed {
            requested: "x".into(),
            reason: "not found".into(),
        };
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn boundary_escape_is_boundary_violation() {
        let err = SandboxPathError::BoundaryEscape {
            requested: "../secret".into(),
            resolved: "/etc/secret".into(),
            project_root: "/home/user/project".into(),
        };
        assert!(err.is_boundary_violation());
    }

    #[test]
    fn symlink_escape_is_boundary_violation() {
        let err = SandboxPathError::SymlinkEscape {
            requested: "link".into(),
            target: "/etc/passwd".into(),
        };
        assert!(err.is_boundary_violation());
    }

    #[test]
    fn resolution_failed_is_not_boundary_violation() {
        let err = SandboxPathError::ResolutionFailed {
            requested: "x".into(),
            reason: "not found".into(),
        };
        assert!(!err.is_boundary_violation());
    }

    // --- edge cases ---

    #[test]
    fn normalize_empty_path() {
        let result = normalize_path(Path::new(""));
        assert_eq!(result, PathBuf::from(""));
    }

    #[test]
    fn normalize_just_dot() {
        let result = normalize_path(Path::new("."));
        // CurDir is skipped, results in empty
        assert_eq!(result, PathBuf::from(""));
    }

    #[test]
    fn normalize_multiple_parent_dirs_past_root() {
        let result = normalize_path(Path::new("/a/../../.."));
        // Can't pop past root: /a → / (pop a) → / (can't pop root) → /
        assert_eq!(result, PathBuf::from("/"));
    }

    #[test]
    fn normalize_mixed_dot_and_dotdot() {
        let result = normalize_path(Path::new("/a/./b/../c/./d/.."));
        assert_eq!(result, PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_relative_deeply_nested() {
        let result = normalize_path(Path::new("a/b/c/../../d"));
        assert_eq!(result, PathBuf::from("a/d"));
    }

    #[test]
    fn validate_empty_path_resolves_to_project_root() {
        let p = standard_policy("/tmp/proj");
        let result = validate_path(&p, "");
        // Empty string → project_root.join("") → project_root
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_boundary_escape_error_info() {
        let p = strict_policy("/home/user/proj");
        let result = validate_path(&p, "/etc/shadow");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_boundary_violation());
        let display = err.to_string();
        assert!(display.contains("/etc/shadow"));
    }

    #[test]
    fn validate_dotdot_escape_blocked_in_standard() {
        let p = standard_policy("/home/user/proj");
        let result = validate_path(&p, "../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn validate_permissive_allows_dotdot() {
        let p = SandboxPolicy::permissive("/home/user/proj");
        let result = validate_path(&p, "../../etc/passwd");
        // Permissive doesn't validate boundaries
        assert!(result.is_ok());
    }

    #[test]
    fn normalize_preserves_trailing_component() {
        let result = normalize_path(Path::new("/a/b/c"));
        assert_eq!(result, PathBuf::from("/a/b/c"));
    }

    #[test]
    fn validate_absolute_within_project_ok() {
        let p = standard_policy("/home/user/proj");
        let result = validate_path(&p, "/home/user/proj/deep/nested/file.rs");
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with("/home/user/proj"));
    }
}
