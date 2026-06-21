//! Path validation and boundary enforcement.

use super::policy::{IsolationLevel, SandboxPolicy, is_never_readable_path};
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
    /// Path matches a credential-bearing location that is blocked at every isolation level.
    SensitivePath { requested: String },
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
            Self::SensitivePath { requested } => {
                write!(
                    f,
                    "Path '{requested}' is blocked as a sensitive credential path"
                )
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
/// For Permissive isolation, returns the path as-is except for bypass-immune
/// credential paths that are blocked at every isolation level.
/// For Standard/Strict isolation:
/// 1. Resolves the path (relative to project_root or absolute)
/// 2. Canonicalizes to resolve symlinks and `..` components
/// 3. Checks the canonical path is within allowed boundaries
///
/// # Errors
///
/// Returns `SandboxPathError` if the path escapes the boundary or can't be resolved.
pub fn validate_path(policy: &SandboxPolicy, path: &str) -> Result<PathBuf, SandboxPathError> {
    // Resolve the raw path
    let raw = Path::new(path);
    let resolved = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        policy.project_root.join(raw)
    };

    if policy.isolation == IsolationLevel::Permissive {
        let sensitive_candidate = sensitive_check_path(&resolved);
        if is_never_readable_path(&resolved) || is_never_readable_path(&sensitive_candidate) {
            return Err(SandboxPathError::SensitivePath {
                requested: path.to_string(),
            });
        }
        return Ok(resolved);
    }

    // Resolve symlinks safely: canonicalize what exists, normalize the rest.
    // This mitigates TOCTOU attacks where a symlink is created between exists() and
    // canonicalize(), and prevents new-file paths from silently escaping via
    // symlinked parent directories. Note: a small race window remains between
    // exists() and canonicalize(); true prevention requires openat2(RESOLVE_BENEATH).
    let canonical = if resolved.exists() {
        resolved
            .canonicalize()
            .map_err(|e| SandboxPathError::ResolutionFailed {
                requested: path.to_string(),
                reason: e.to_string(),
            })?
    } else {
        // For new files: canonicalize the nearest existing ancestor to resolve
        // symlinks in parent components, then append the remaining path segments.
        canonicalize_parent_and_append(&resolved)?
    };

    if is_never_readable_path(&canonical) {
        return Err(SandboxPathError::SensitivePath {
            requested: path.to_string(),
        });
    }

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

fn sensitive_check_path(path: &Path) -> PathBuf {
    if path.exists()
        && let Ok(canonical) = path.canonicalize()
    {
        return canonical;
    }
    canonicalize_parent_and_append(path).unwrap_or_else(|_| normalize_path(path))
}

/// Normalize path without filesystem access (for non-existent files).
///
/// Resolves `.` and `..` components lexically. This doesn't follow symlinks
/// but prevents obvious directory traversal attacks.
pub fn normalize_path(path: &Path) -> PathBuf {
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

/// Canonicalize the nearest existing ancestor directory, then append the remaining
/// path segments. This safely resolves symlinks in parent components for new-file paths.
///
/// Example: if `/home/user/proj/subdir` exists but `newfile.txt` doesn't, canonicalize
/// `/home/user/proj/subdir` then append `newfile.txt`.
pub fn canonicalize_parent_and_append(path: &Path) -> Result<PathBuf, SandboxPathError> {
    let mut current = path.to_path_buf();
    let mut suffix = Vec::new();

    // Walk up until we find an existing ancestor
    while !current.exists() {
        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        } else {
            break;
        }
        if !current.pop() {
            break;
        }
    }

    // Canonicalize the existing ancestor
    let canonical_base = if current.exists() {
        current
            .canonicalize()
            .map_err(|e| SandboxPathError::ResolutionFailed {
                requested: path.display().to_string(),
                reason: e.to_string(),
            })?
    } else {
        // Fallback: couldn't find any existing ancestor, normalize the whole path
        return Ok(normalize_path(path));
    };

    // Append the suffix in reverse order (we collected bottom-up)
    let mut result = canonical_base;
    for segment in suffix.into_iter().rev() {
        result.push(segment);
    }

    Ok(result)
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

    // ── Permissive isolation ─────────────────────────────────────────────

    #[test]
    fn permissive_isolation_path_validation() {
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

    #[test]
    fn permissive_isolation_blocks_sensitive_credential_paths() {
        let p = SandboxPolicy::permissive("/home/user/project");

        for path in [
            "/home/user/.ssh/id_rsa",
            "/home/user/.aws/credentials",
            "/etc/shadow",
        ] {
            let result = validate_path(&p, path);
            assert!(result.is_err(), "permissive must block {path}");
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("sensitive credential path"),
                "denial should name sensitive path policy"
            );
        }
    }

    // ── Standard isolation ───────────────────────────────────────────────

    #[test]
    fn standard_isolation_path_validation() {
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

    // ── Strict isolation ─────────────────────────────────────────────────

    #[test]
    fn strict_isolation_path_validation() {
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
