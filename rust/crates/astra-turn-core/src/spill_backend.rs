//! Spill backend: persists oversized section content outside the prompt so
//! the optimizer can replace it with a lightweight `SpillReference`.
//!
//! The pipeline itself stays pure — it asks a backend to store bytes and
//! hands back only a reference (path/URI + original token estimate). The
//! callsite is `optimize()`; without a backend the optimizer keeps its
//! conservative behaviour (preserve content, emit a skipped-optimization
//! trace entry).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Storage sink for spilled section content.
///
/// Implementations MUST be thread-safe (`Send + Sync`) because a single
/// pipeline session may be driven from multiple async tasks, and backends
/// are shared via `&dyn SpillBackend`.
pub trait SpillBackend: Send + Sync {
    /// Persist `bytes` associated with a structural `key_hint` (kind + turn
    /// descriptor) and return an opaque locator — typically a filesystem
    /// path, but any stable string a downstream dereferencer can resolve.
    ///
    /// `key_hint` is advisory: backends may use it to build human-readable
    /// filenames, but are free to ignore it.
    fn store(&self, key_hint: &str, bytes: &[u8]) -> io::Result<String>;
}

/// Filesystem-backed spill store. Writes each payload to
/// `{root}/{sanitized_key_hint}-{counter}.txt`. Counter is monotonic per
/// backend instance so concurrent stores never collide.
#[derive(Debug)]
pub struct FileSystemSpillBackend {
    root: PathBuf,
    counter: AtomicU64,
}

impl FileSystemSpillBackend {
    /// Create a backend rooted at `dir`. The directory is created lazily on
    /// first `store` call.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            root: dir.into(),
            counter: AtomicU64::new(0),
        }
    }

    /// Directory this backend writes to.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl SpillBackend for FileSystemSpillBackend {
    fn store(&self, key_hint: &str, bytes: &[u8]) -> io::Result<String> {
        // Create root on first use so constructing a backend is cheap and
        // test-friendly (doesn't touch disk until spill actually happens).
        fs::create_dir_all(&self.root)?;

        let sanitized = sanitize_key(key_hint);
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let filename = format!("{sanitized}-{n}.txt");
        let path = self.root.join(&filename);
        fs::write(&path, bytes)?;
        Ok(path.to_string_lossy().into_owned())
    }
}

fn sanitize_key(key: &str) -> String {
    let cleaned: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "spill".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn filesystem_backend_writes_and_returns_path() {
        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let path = backend
            .store("s1-turn3-ProjectContext", b"payload bytes")
            .expect("store succeeds");

        let contents = fs::read(&path).unwrap();
        assert_eq!(contents, b"payload bytes");
        assert!(path.contains("s1-turn3-ProjectContext"));
    }

    #[test]
    fn filesystem_backend_produces_unique_paths_for_same_key() {
        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let a = backend.store("same", b"A").unwrap();
        let b = backend.store("same", b"B").unwrap();
        assert_ne!(a, b, "counter must disambiguate identical key hints");
        assert_eq!(fs::read(&a).unwrap(), b"A");
        assert_eq!(fs::read(&b).unwrap(), b"B");
    }

    #[test]
    fn filesystem_backend_sanitizes_unsafe_key_chars() {
        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let path = backend
            .store("evil/../key with spaces", b"x")
            .expect("store succeeds");
        assert!(!path.contains(".."));
        assert!(!path.contains(' '));
    }

    #[test]
    fn filesystem_backend_creates_directory_lazily() {
        let parent = TempDir::new().unwrap();
        let nested = parent.path().join("nested/spill");
        assert!(!nested.exists(), "precondition: dir does not exist yet");
        let backend = FileSystemSpillBackend::new(&nested);
        // Construction alone should not create the dir.
        assert!(!nested.exists());
        backend.store("k", b"v").unwrap();
        assert!(nested.is_dir(), "dir should be created on first store");
    }
}
