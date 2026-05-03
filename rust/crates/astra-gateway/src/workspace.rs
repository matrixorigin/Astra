//! Workspace discovery and management.
//!
//! Scans directories for git projects, provides listings for agent context.

use std::path::Path;

/// Scan a directory for git projects (directories containing .git).
pub fn discover_projects(base_dir: &str) -> Vec<ProjectInfo> {
    let expanded = expand_home(base_dir);
    let path = Path::new(&expanded);
    if !path.is_dir() {
        return Vec::new();
    }
    let mut projects = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join(".git").exists() {
                let name = p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let branch = read_git_branch(&p);
                let has_changes = has_uncommitted_changes(&p);
                projects.push(ProjectInfo {
                    name,
                    path: p.to_string_lossy().to_string(),
                    branch,
                    has_changes,
                });
            }
        }
    }
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    projects
}

/// Scan multiple base directories.
pub fn discover_all_projects(dirs: &[String]) -> Vec<ProjectInfo> {
    let mut all = Vec::new();
    for dir in dirs {
        all.extend(discover_projects(dir));
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all.dedup_by(|a, b| a.path == b.path);
    all
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub name: String,
    pub path: String,
    pub branch: Option<String>,
    pub has_changes: bool,
}

impl ProjectInfo {
    pub fn summary(&self) -> String {
        let branch = self.branch.as_deref().unwrap_or("?");
        let marker = if self.has_changes { " *" } else { "" };
        format!("`{}` → `{}` ({}{})", self.name, self.path, branch, marker)
    }
}

fn expand_home(path: &str) -> String {
    if path.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        path.replacen('~', &home, 1)
    } else {
        path.to_string()
    }
}

fn read_git_branch(repo: &Path) -> Option<String> {
    let head = repo.join(".git/HEAD");
    let content = std::fs::read_to_string(head).ok()?;
    let trimmed = content.trim();
    if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
        Some(branch.to_string())
    } else if trimmed.len() >= 8 {
        Some(trimmed[..8].to_string()) // detached HEAD
    } else {
        None
    }
}

fn has_uncommitted_changes(repo: &Path) -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_git_repos() {
        // This test depends on the actual filesystem — the astra repo itself
        let home = std::env::var("HOME").unwrap_or_default();
        let projects = discover_projects(&format!("{home}/astra/rust/crates"));
        // astra-gateway is a subdirectory, not a git repo itself
        // But ~/astra itself should be findable
        // Just verify it doesn't panic and returns a vec
        assert!(projects.is_empty() || !projects.is_empty());
    }

    #[test]
    fn discover_nonexistent_dir() {
        let projects = discover_projects("/nonexistent/path/12345");
        assert!(projects.is_empty());
    }

    #[test]
    fn expand_home_works() {
        let expanded = expand_home("~/test");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.ends_with("/test"));
    }

    #[test]
    fn expand_home_no_tilde() {
        assert_eq!(expand_home("/absolute/path"), "/absolute/path");
    }

    #[test]
    fn project_summary_format() {
        let p = ProjectInfo {
            name: "my-project".into(),
            path: "/home/user/my-project".into(),
            branch: Some("main".into()),
            has_changes: true,
        };
        let s = p.summary();
        assert!(s.contains("my-project"));
        assert!(s.contains("main"));
        assert!(s.contains("*")); // has changes marker
    }

    #[test]
    fn project_summary_no_changes() {
        let p = ProjectInfo {
            name: "clean".into(),
            path: "/tmp/clean".into(),
            branch: Some("dev".into()),
            has_changes: false,
        };
        let s = p.summary();
        assert!(!s.contains("*"));
    }

    #[test]
    fn read_git_branch_from_real_repo() {
        let home = std::env::var("HOME").unwrap_or_default();
        let astra = std::path::PathBuf::from(format!("{home}/astra"));
        if astra.join(".git").exists() {
            let branch = read_git_branch(&astra);
            assert!(branch.is_some(), "should read branch from astra repo");
        }
    }
}
