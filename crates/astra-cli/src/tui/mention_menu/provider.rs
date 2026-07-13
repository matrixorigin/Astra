//! File-listing abstraction for the `@`-mention menu.
//!
//! The menu does not read the filesystem directly; it talks to a
//! [`FileProvider`] so tests can inject deterministic fixtures and the
//! real code can scan the working directory via `std::fs`.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

#[cfg(not(test))]
const CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(test)]
const CACHE_REFRESH_INTERVAL: Duration = Duration::from_millis(25);

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
pub(crate) trait FileProvider: std::fmt::Debug + Send + Sync {
    fn list(&self, relative_dir: &str) -> Vec<FileEntry>;

    fn revision(&self) -> u64 {
        0
    }

    fn is_loading(&self, _relative_dir: &str) -> bool {
        false
    }

    fn load_error(&self, _relative_dir: &str) -> Option<String> {
        None
    }

    fn poll_refresh(&self, _relative_dir: &str) {}
}

// ── Filesystem-backed provider ────────────────────────────────────

#[derive(Debug, Clone)]
struct CachedListing {
    loaded_at: Instant,
    result: Result<Vec<FileEntry>, String>,
}

/// Reads direct children of `root / relative_dir` without recursion.
/// Skips dot entries and names containing whitespace (would confuse the
/// subsequent plain-text `@path ` insertion).
#[derive(Debug, Clone)]
pub(crate) struct FsFileProvider {
    /// Max entries to return from a single `list` call. Keeps the menu
    /// responsive in huge directories.
    max_entries: Arc<AtomicUsize>,
    listings: Arc<RwLock<HashMap<String, CachedListing>>>,
    requested: Arc<Mutex<HashSet<String>>>,
    request_tx: std::sync::mpsc::Sender<String>,
    revision: Arc<AtomicU64>,
}

impl FsFileProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let max_entries = Arc::new(AtomicUsize::new(200));
        let listings = Arc::new(RwLock::new(HashMap::new()));
        let requested = Arc::new(Mutex::new(HashSet::new()));
        let revision = Arc::new(AtomicU64::new(0));
        let (request_tx, request_rx) = std::sync::mpsc::channel::<String>();

        let worker_root = root.clone();
        let worker_max_entries = max_entries.clone();
        let worker_listings = listings.clone();
        let worker_requested = requested.clone();
        let worker_revision = revision.clone();
        let _ = std::thread::Builder::new()
            .name("astra-mention-files".to_string())
            .spawn(move || {
                while let Ok(relative_dir) = request_rx.recv() {
                    let listing = scan_directory(
                        &worker_root,
                        &relative_dir,
                        worker_max_entries.load(Ordering::Relaxed),
                    );
                    worker_listings
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(
                            relative_dir.clone(),
                            CachedListing {
                                loaded_at: Instant::now(),
                                result: listing,
                            },
                        );
                    worker_requested
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&relative_dir);
                    worker_revision.fetch_add(1, Ordering::Release);
                }
            });

        Self {
            max_entries,
            listings,
            requested,
            request_tx,
            revision,
        }
    }

    pub fn with_max_entries(self, n: usize) -> Self {
        self.max_entries.store(n, Ordering::Relaxed);
        self
    }

    fn request_if_stale(&self, relative_dir: &str) {
        let is_fresh = self
            .listings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(relative_dir)
            .is_some_and(|listing| listing.loaded_at.elapsed() < CACHE_REFRESH_INTERVAL);
        if is_fresh {
            return;
        }
        let mut requested = self
            .requested
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !requested.insert(relative_dir.to_string()) {
            return;
        }
        if self.request_tx.send(relative_dir.to_string()).is_err() {
            requested.remove(relative_dir);
            self.listings
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    relative_dir.to_string(),
                    CachedListing {
                        loaded_at: Instant::now(),
                        result: Err("file index worker is unavailable".to_string()),
                    },
                );
            self.revision.fetch_add(1, Ordering::Release);
        }
    }
}

impl FileProvider for FsFileProvider {
    fn list(&self, relative_dir: &str) -> Vec<FileEntry> {
        self.request_if_stale(relative_dir);
        self.listings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(relative_dir)
            .and_then(|listing| listing.result.as_ref().ok())
            .cloned()
            .unwrap_or_default()
    }

    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn is_loading(&self, relative_dir: &str) -> bool {
        self.requested
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(relative_dir)
    }

    fn load_error(&self, relative_dir: &str) -> Option<String> {
        self.listings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(relative_dir)
            .and_then(|listing| listing.result.as_ref().err())
            .cloned()
    }

    fn poll_refresh(&self, relative_dir: &str) {
        self.request_if_stale(relative_dir);
    }
}

fn scan_directory(
    root: &Path,
    relative_dir: &str,
    max_entries: usize,
) -> Result<Vec<FileEntry>, String> {
    if max_entries == 0 {
        return Ok(Vec::new());
    }
    let dir = if relative_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative_dir)
    };
    let read_dir = std::fs::read_dir(&dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.starts_with('.') || name.chars().any(char::is_whitespace) {
            continue;
        }
        let kind = match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => FileKind::Dir,
            Ok(_) => FileKind::File,
            Err(_) => continue,
        };
        entries.push(FileEntry {
            path: join_rel(relative_dir, name),
            kind,
        });
        if entries.len() >= max_entries {
            break;
        }
    }
    entries.sort_by(|a, b| match (a.kind, b.kind) {
        (FileKind::Dir, FileKind::File) => std::cmp::Ordering::Less,
        (FileKind::File, FileKind::Dir) => std::cmp::Ordering::Greater,
        _ => a.path.cmp(&b.path),
    });
    Ok(entries)
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
    use super::{
        CACHE_REFRESH_INTERVAL, FileKind, FileProvider, FsFileProvider, StaticFileProvider,
    };
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

    #[tokio::test]
    async fn fs_provider_loads_once_off_thread_then_serves_cached_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join(".hidden"), "").unwrap();

        let p = FsFileProvider::new(tmp.path());
        let _ = p.list("");
        crate::tests::wait_until(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(10),
            || p.revision() > 0 && !p.is_loading(""),
        )
        .await
        .expect("background directory scan");
        let entries = p.list("");
        let names: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names.len(), 2, "dot-entry skipped: {names:?}");
        // dirs ordered before files.
        assert_eq!(entries[0].path, "d");
        assert_eq!(entries[0].kind, FileKind::Dir);
        assert_eq!(entries[1].path, "a.txt");
        assert_eq!(entries[1].kind, FileKind::File);

        std::fs::remove_file(tmp.path().join("a.txt")).unwrap();
        std::fs::remove_dir(tmp.path().join("d")).unwrap();
        assert_eq!(
            p.list(""),
            entries,
            "hot-path filtering must use the cached snapshot, not call read_dir again"
        );

        std::fs::write(tmp.path().join("new.rs"), "").unwrap();
        let prior_revision = p.revision();
        tokio::time::sleep(CACHE_REFRESH_INTERVAL + std::time::Duration::from_millis(5)).await;
        p.poll_refresh("");
        crate::tests::wait_until(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(10),
            || p.revision() > prior_revision && !p.is_loading(""),
        )
        .await
        .expect("stale snapshot refresh");
        let refreshed = p.list("");
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].path, "new.rs");
    }

    #[tokio::test]
    async fn fs_provider_missing_dir_finishes_with_an_explicit_error() {
        let p = FsFileProvider::new(Path::new("/nonexistent/path/should/not/exist"));
        let _ = p.list("");
        crate::tests::wait_until(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(10),
            || p.revision() > 0 && !p.is_loading(""),
        )
        .await
        .expect("failed scan should still converge");
        assert!(p.list("").is_empty());
        assert!(p.load_error("").is_some());
    }
}
