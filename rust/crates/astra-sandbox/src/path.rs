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

    // ── Permissive mode ────────────────────────────────────────────────��

    #[test]
    fn permissive_mode_path_validation() {
        let p = SandboxPolicy::permissive("/home/user/project");
        // Allows absolute paths
        let result = validate_path(&p, "/etc/passwd");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("/etc/passwd"));
        // Resolves relative
        let result = validate_path(&p, "src/main.rs");
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            PathBuf::from("/home/user/project/src/main.rs")
        );
        // Allows dotdot escape
        let result = validate_path(&p, "../../etc/passwd");
        assert!(result.is_ok());
    }

    // ── Standard mode ────────────────────────────────────────────────────

    #[test]
    fn standard_mode_path_validation() {
        let p = standard_policy("/home/user/proj");
        // Allows relative within project
        assert!(validate_path(&p, "subdir/file.txt").is_ok());
        // Allows absolute within project
        assert!(validate_path(&p, "/home/user/proj/deep/nested/file.rs").is_ok());
        // Allows /tmp
        assert!(validate_path(&p, "/tmp/build-output.tar.gz").is_ok());
        // Allows empty → resolves to project root
        assert!(validate_path(&p, "").is_ok());
        // Blocks dotdot escape
        for bad in ["../../../etc/passwd", "../../etc/passwd"] {
            let result = validate_path(&p, bad);
            assert!(result.is_err(), "should block: {bad}");
        }
        // Blocks absolute outside root
        assert!(validate_path(&p, "/etc/shadow").is_err());
    }

    // ── Strict mode ──────────────────────────────────────────────────────

    #[test]
    fn strict_mode_path_validation() {
        let p = strict_policy("/home/user/project");
        // Blocks /var/tmp
        assert!(validate_path(&p, "/var/tmp/secret").is_err());
        // Allows /tmp
        assert!(validate_path(&p, "/tmp/build.log").is_ok());
    }

    // ── Path normalization ───────────────────────────────────────────────

    #[test]
    fn path_normalization_rules() {
        let cases: &[(&str, &str)] = &[
            // .. removal
            ("/home/user/project/../../etc/passwd", "/home/etc/passwd"),
            ("/a/../../..", "/"),
            (".", ""),
            ("", ""),
            ("/a/b/c", "/a/b/c"),
            // . removal
            ("/home/user/./project/./src", "/home/user/project/src"),
            // Mixed . and ..
            ("/a/./b/../c/./d/..", "/a/c"),
            // Relative
            ("a/b/../c", "a/c"),
            ("a/b/c/../../d", "a/d"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_path(Path::new(input)),
                PathBuf::from(expected),
                "input: {input}"
            );
        }
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
    fn error_display_and_classification() {
        // Display
        let boundary = SandboxPathError::BoundaryEscape {
            requested: "../secret".into(),
            resolved: "/etc/secret".into(),
            project_root: "/home/user/project".into(),
        };
        let msg = boundary.to_string();
        assert!(msg.contains("escapes project boundary"));
        assert!(msg.contains("../secret"));

        let symlink = SandboxPathError::SymlinkEscape {
            requested: "link".into(),
            target: "/etc/passwd".into(),
        };
        assert!(symlink.to_string().contains("outside boundary"));

        let resolution = SandboxPathError::ResolutionFailed {
            requested: "x".into(),
            reason: "not found".into(),
        };
        assert!(resolution.to_string().contains("not found"));

        // Classification
        assert!(boundary.is_boundary_violation());
        assert!(symlink.is_boundary_violation());
        assert!(!resolution.is_boundary_violation());
    }
}
