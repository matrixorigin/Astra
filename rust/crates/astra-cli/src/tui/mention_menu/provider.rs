//! File-listing abstraction for the `@`-mention menu.
//!
//! The menu does not read the filesystem directly; it talks to a
//! [`FileProvider`] so tests can inject deterministic fixtures and the
//! real code can scan the working directory via `std::fs`.

#![allow(dead_code)]

use std::path::PathBuf;

/// A single file or directory shown in the mention menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileEntry {
    /// Path relative to the provider's root (cwd).
    pub path: String,
    pub kind: FileKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    File,
    Dir,
}

/// Listing abstraction. Callers pass a relative directory (possibly "")
/// and get a flat list of its immediate children.
pub(crate) trait FileProvider: std::fmt::Debug {
    fn list(&self, relative_dir: &str) -> Vec<FileEntry>;
}

// ── Filesystem-backed provider ────────────────────────────────────

/// Reads direct children of `root / relative_dir` without recursion.
/// Skips dot entries and names containing whitespace (would confuse the
/// subsequent plain-text `@path ` insertion).
#[derive(Debug, Clone)]
pub(crate) struct FsFileProvider {
    root: PathBuf,
    /// Max entries to return from a single `list` call. Keeps the menu
    /// responsive in huge directories.
    max_entries: usize,
}

impl FsFileProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_entries: 200,
        }
    }

    pub fn with_max_entries(mut self, n: usize) -> Self {
        self.max_entries = n;
        self
    }
}

impl FileProvider for FsFileProvider {
    fn list(&self, relative_dir: &str) -> Vec<FileEntry> {
        let dir = if relative_dir.is_empty() {
            self.root.clone()
        } else {
            self.root.join(relative_dir)
        };

        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };

        let mut entries: Vec<FileEntry> = Vec::new();
        for entry in read_dir.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            if name.chars().any(char::is_whitespace) {
                // Path insertion is whitespace-delimited; skip noisy names.
                continue;
            }

            let kind = match entry.file_type() {
                Ok(ft) if ft.is_dir() => FileKind::Dir,
                Ok(_) => FileKind::File,
                Err(_) => continue,
            };

            let rel = join_rel(relative_dir, name);
            entries.push(FileEntry { path: rel, kind });

            if entries.len() >= self.max_entries {
                break;
            }
        }

        // Stable order: directories first, then files, each alphabetical.
        entries.sort_by(|a, b| match (a.kind, b.kind) {
            (FileKind::Dir, FileKind::File) => std::cmp::Ordering::Less,
            (FileKind::File, FileKind::Dir) => std::cmp::Ordering::Greater,
            _ => a.path.cmp(&b.path),
        });
        entries
    }
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

// ── Fixed-list provider for tests ─────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct StaticFileProvider {
    pub entries: Vec<FileEntry>,
}

impl StaticFileProvider {
    pub fn new(entries: Vec<FileEntry>) -> Self {
        Self { entries }
    }

    pub fn with_root_listing(pairs: &[(&str, FileKind)]) -> Self {
        let entries = pairs
            .iter()
            .map(|(p, k)| FileEntry {
                path: (*p).to_string(),
                kind: *k,
            })
            .collect();
        Self { entries }
    }
}

impl FileProvider for StaticFileProvider {
    /// Returns entries whose parent matches `relative_dir`. Matching is
    /// path-prefix based: an entry at "src/lib/mod.rs" is returned for
    /// `relative_dir = "src/lib"` as the final segment "mod.rs".
    fn list(&self, relative_dir: &str) -> Vec<FileEntry> {
        let dir = relative_dir.trim_matches('/');
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };

        self.entries
            .iter()
            .filter_map(|e| {
                if prefix.is_empty() {
                    // Root listing: keep entries with no slash.
                    if !e.path.contains('/') {
                        Some(e.clone())
                    } else {
                        None
                    }
                } else if let Some(rest) = e.path.strip_prefix(&prefix) {
                    // Direct child only — no nested path fragments.
                    if !rest.contains('/') {
                        Some(FileEntry {
                            path: e.path.clone(),
                            kind: e.kind,
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Helper used by consumers that want a `&dyn FileProvider`.
pub(crate) fn as_dyn(p: &impl FileProvider) -> &dyn FileProvider {
    p as &dyn FileProvider
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn static_provider_lists_root_entries() {
        let p = StaticFileProvider::with_root_listing(&[
            ("src", FileKind::Dir),
            ("README.md", FileKind::File),
            ("src/main.rs", FileKind::File),
        ]);
        let root = p.list("");
        let names: Vec<&str> = root.iter().map(|e| e.path.as_str()).collect();
        assert!(names.contains(&"src"));
        assert!(names.contains(&"README.md"));
        assert!(
            !names.contains(&"src/main.rs"),
            "nested file excluded at root"
        );
    }

    #[test]
    fn static_provider_lists_subdir_entries() {
        let p = StaticFileProvider::with_root_listing(&[
            ("src", FileKind::Dir),
            ("src/main.rs", FileKind::File),
            ("src/lib", FileKind::Dir),
            ("src/lib/mod.rs", FileKind::File),
        ]);
        let in_src = p.list("src");
        let names: Vec<&str> = in_src.iter().map(|e| e.path.as_str()).collect();
        assert!(names.contains(&"src/main.rs"));
        assert!(names.contains(&"src/lib"));
        assert!(!names.contains(&"src/lib/mod.rs"), "nested excluded");
    }

    #[test]
    fn fs_provider_reads_real_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join(".hidden"), "").unwrap();

        let p = FsFileProvider::new(tmp.path());
        let entries = p.list("");
        let names: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names.len(), 2, "dot-entry skipped: {names:?}");
        // dirs ordered before files.
        assert_eq!(entries[0].path, "d");
        assert_eq!(entries[0].kind, FileKind::Dir);
        assert_eq!(entries[1].path, "a.txt");
        assert_eq!(entries[1].kind, FileKind::File);
    }

    #[test]
    fn fs_provider_missing_dir_returns_empty() {
        let p = FsFileProvider::new(Path::new("/nonexistent/path/should/not/exist"));
        assert!(p.list("").is_empty());
    }
}
