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
    /// Returns `true` when the error is an expandable sandbox boundary
    /// violation (not a resolution failure or bypass-immune sensitive path).
    /// Callers can use this to distinguish "needs user authorization" from
    /// "path is broken" and "path is never readable".
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

    // Resolve existing components before normalizing missing tails. Authorization
    // remains subject to filesystem changes after this check.
    let canonical = canonicalize_parent_and_append(&resolved)?;

    if is_never_readable_path(&resolved) || is_never_readable_path(&canonical) {
        return Err(SandboxPathError::SensitivePath {
            requested: path.to_string(),
        });
    }

    // Check boundary
    if if raw.is_absolute() {
        policy.is_resolved_path_allowed(&canonical)
    } else {
        policy.is_within_project_root(&canonical)
    } {
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
    let failure = |error: std::io::Error| SandboxPathError::ResolutionFailed {
        requested: path.display().to_string(),
        reason: error.to_string(),
    };
    match path.canonicalize() {
        Ok(canonical) => return Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(failure(error)),
    }

    let mut result = if path.is_absolute() {
        PathBuf::new()
    } else {
        Path::new(".").canonicalize().map_err(failure)?
    };
    for component in path.components() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
        match result.canonicalize() {
            Ok(canonical) => result = canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is not a new file. Do not authorize its
                // spelling as though it were an ordinary missing component.
                match std::fs::symlink_metadata(&result) {
                    Ok(metadata) if metadata.is_symlink() => return Err(failure(error)),
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(failure(error));
                    }
                    _ => {}
                }
            }
            Err(error) => return Err(failure(error)),
        }
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

    #[test]
    fn relative_workspace_resolves_missing_targets_from_the_current_directory() {
        let root = Path::new(".").canonicalize().unwrap();
        let name = format!("astra-missing-{}", uuid::Uuid::new_v4());
        let policy = SandboxPolicy::for_project(".");
        assert_eq!(validate_path(&policy, ".").unwrap(), root);
        for input in [name.clone(), format!("{name}/new/deep.txt")] {
            assert!(!root.join(&input).exists());
            let expected = root.join(&input);
            assert_eq!(validate_path(&policy, &input).unwrap(), expected);
            assert_eq!(
                validate_path(&SandboxPolicy::for_project(&root), &input).unwrap(),
                expected
            );
        }
        let outside = format!("../{name}/new.txt");
        assert_eq!(
            canonicalize_parent_and_append(Path::new(&outside)).unwrap(),
            root.parent().unwrap_or(&root).join(&name).join("new.txt")
        );
        if root.parent().is_some() {
            assert!(matches!(
                validate_path(&policy, &outside),
                Err(SandboxPathError::BoundaryEscape { .. })
            ));
        }
    }

    #[test]
    fn relative_authority_stays_in_workspace_despite_absolute_grants() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("marker"), "outside").unwrap();
        for mut policy in [
            SandboxPolicy::for_project(&root),
            SandboxPolicy::strict(&root),
        ] {
            policy.add_allowed_path(&outside).unwrap();
            for input in ["../outside/marker", "../outside/new/deep/file"] {
                assert!(
                    matches!(
                        validate_path(&policy, input),
                        Err(SandboxPathError::BoundaryEscape { .. })
                    ),
                    "{input}"
                );
            }
            assert!(validate_path(&policy, outside.join("marker").to_str().unwrap()).is_ok());
            assert_eq!(
                validate_path(&policy, "new/deep/../file").unwrap(),
                root.canonicalize().unwrap().join("new/file")
            );
        }
        assert!(validate_path(&SandboxPolicy::permissive(&root), "../outside/marker").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolution_precedes_parent_normalization_and_missing_tails() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let outside = temp.path().join("outside/child");
        std::fs::create_dir_all(root.join("inside")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("external")).unwrap();
        symlink(root.join("inside"), root.join("internal")).unwrap();
        symlink(&root, temp.path().join("alias")).unwrap();
        symlink(root.join("absent"), root.join("dangling")).unwrap();
        symlink(root.join("loop"), root.join("loop")).unwrap();
        let policy = SandboxPolicy::for_project(temp.path().join("alias"));
        for input in [
            "external",
            "external/../marker",
            "external/new/deep/file",
            "missing/../external/new",
        ] {
            assert!(
                matches!(
                    validate_path(&policy, input),
                    Err(SandboxPathError::BoundaryEscape { .. })
                ),
                "{input}"
            );
        }
        assert_eq!(
            validate_path(&policy, "internal/new/deep").unwrap(),
            root.canonicalize().unwrap().join("inside/new/deep")
        );
        for input in ["dangling", "dangling/new", "loop", "loop/new"] {
            assert!(
                matches!(
                    validate_path(&policy, input),
                    Err(SandboxPathError::ResolutionFailed { .. })
                ),
                "{input}"
            );
        }
    }

    // ── Permissive isolation ─────────────────────────────────────────────

    #[test]
    fn permissive_isolation_path_validation() {
        let p = SandboxPolicy::permissive("/home/user/project");
        // Allows arbitrary absolute paths (non-sensitive).
        let result = validate_path(&p, "/var/data/file");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("/var/data/file"));
        // Resolves relative
        let result = validate_path(&p, "src/main.rs");
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            PathBuf::from("/home/user/project/src/main.rs")
        );
        // Allows dotdot escape to non-sensitive paths
        let result = validate_path(&p, "../../var/data");
        assert!(result.is_ok());
        // System account files are blocked at every isolation level —
        // /etc/passwd is no longer a valid permissive-mode target.
        let result = validate_path(&p, "/etc/passwd");
        assert!(
            result.is_err(),
            "/etc/passwd must be blocked in permissive mode"
        );
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
        let sensitive = SandboxPathError::SensitivePath {
            requested: "~/.ssh/id_rsa".into(),
        };
        assert!(!sensitive.is_boundary_violation());
        assert!(!resolution.is_boundary_violation());
    }
}
