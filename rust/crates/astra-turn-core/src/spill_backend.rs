//! Spill backend: persists oversized section content outside the prompt so
//! the optimizer can replace it with a lightweight `SpillReference`.
//!
//! The pipeline itself stays pure — it asks a backend to store bytes and
//! hands back only a reference (path/URI + original token estimate). The
//! callsite is `optimize()`; without a backend the optimizer keeps its
//! conservative behaviour (preserve content, emit a skipped-optimization
//! trace entry).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

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

    /// Resolve a `locator` previously returned by [`SpillBackend::store`]
    /// back to the original bytes.
    ///
    /// This is the dual of `store` and is the foundation for Phase 12
    /// session-resume / on-demand rehydration of spilled `SectionArtifact::
    /// SpillReference` payloads.
    ///
    /// Default implementation returns `io::ErrorKind::Unsupported` so
    /// existing backends that only implement `store` keep compiling — but
    /// they will fail closed if a consumer tries to rehydrate. Real
    /// backends SHOULD override this.
    fn load(&self, locator: &str) -> io::Result<Vec<u8>> {
        let _ = locator;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SpillBackend::load not implemented for this backend",
        ))
    }
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

    /// Resolve a locator (filesystem path returned by `store`) back to its
    /// bytes. Fails closed if the path is outside this backend's `root` —
    /// prevents a compromised pipeline from reading arbitrary files via
    /// crafted `SpillReference` locators.
    fn load(&self, locator: &str) -> io::Result<Vec<u8>> {
        let path = PathBuf::from(locator);

        // Resolve both to canonical form so `..` traversal / symlink
        // escape is rejected before we ever read.
        let canon_path = fs::canonicalize(&path)?;
        let canon_root = fs::canonicalize(&self.root)?;
        if !canon_path.starts_with(&canon_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "spill locator escapes backend root",
            ));
        }

        fs::read(&canon_path)
    }
}

impl FileSystemSpillBackend {
    /// Delete files in this backend's root whose modification time is
    /// older than `max_age`. Returns the number of files removed.
    ///
    /// Errors on individual files are logged via `tracing` (when enabled)
    /// but do not abort the sweep — a single corrupt/locked file must not
    /// block GC progress for the rest of the directory.
    pub fn prune_older_than(&self, max_age: Duration) -> io::Result<u64> {
        let cutoff = SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut removed: u64 = 0;
        let entries = match fs::read_dir(&self.root) {
            Ok(it) => it,
            // If the directory does not exist yet (no spill ever happened)
            // treat as a clean sweep: nothing to do.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if modified < cutoff {
                if fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

/// Default scheme used by [`SpillRegistry`] when a locator is a bare path
/// (no `scheme://` prefix). Legacy `SpillReference` values written before
/// the registry was introduced fall back to this scheme.
pub const DEFAULT_SCHEME: &str = "file";

/// Parse a locator of the form `scheme://path` into its pieces. Returns
/// `(DEFAULT_SCHEME, locator)` if no scheme is present, preserving
/// backwards compatibility with legacy payloads.
#[must_use]
pub fn parse_locator(locator: &str) -> (&str, &str) {
    match locator.split_once("://") {
        Some((scheme, rest)) if !scheme.is_empty() => (scheme, rest),
        _ => (DEFAULT_SCHEME, locator),
    }
}

/// Build a locator string of the form `scheme://path`.
#[must_use]
pub fn build_locator(scheme: &str, path: &str) -> String {
    format!("{scheme}://{path}")
}

/// Scheme-based registry of [`SpillBackend`]s.
///
/// The registry is the Phase-12 answer to "given a `SpillReference` locator
/// produced by an earlier turn (possibly in another process), which backend
/// resolves it?". Each backend is registered under a short scheme string
/// (e.g. `"file"`, `"s3"`); locators use `scheme://path` form.
///
/// Legacy locators (bare paths written before the registry existed) route
/// to the [`DEFAULT_SCHEME`] backend, which is typically a
/// [`FileSystemSpillBackend`]. This keeps on-disk payloads from earlier
/// sessions readable after an upgrade.
#[derive(Clone, Default)]
pub struct SpillRegistry {
    backends: HashMap<String, Arc<dyn SpillBackend>>,
}

impl std::fmt::Debug for SpillRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpillRegistry")
            .field("schemes", &self.backends.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SpillRegistry {
    /// Create an empty registry — `load` will fail closed until a backend
    /// is registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `backend` for `scheme`. Replaces any backend previously
    /// bound to the same scheme.
    pub fn register(&mut self, scheme: impl Into<String>, backend: Arc<dyn SpillBackend>) {
        self.backends.insert(scheme.into(), backend);
    }

    /// Look up the backend bound to `scheme`.
    #[must_use]
    pub fn resolve(&self, scheme: &str) -> Option<Arc<dyn SpillBackend>> {
        self.backends.get(scheme).cloned()
    }

    /// Resolve `locator` (a `scheme://path` or legacy bare path) through
    /// the registered backends. Returns `NotFound` if the scheme is not
    /// registered.
    pub fn load(&self, locator: &str) -> io::Result<Vec<u8>> {
        let (scheme, path) = parse_locator(locator);
        let backend = self.resolve(scheme).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no spill backend registered for scheme '{scheme}'"),
            )
        })?;
        backend.load(path)
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

    #[test]
    fn filesystem_backend_load_roundtrips_stored_bytes() {
        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let locator = backend
            .store("s1-turn3-ProjectContext", b"round trip payload")
            .expect("store succeeds");

        let loaded = backend.load(&locator).expect("load succeeds");
        assert_eq!(loaded, b"round trip payload");
    }

    #[test]
    fn filesystem_backend_load_rejects_paths_outside_root() {
        // Create a backend rooted inside `inside_root`, then write a file
        // OUTSIDE that root and try to load it by its absolute path.
        // The backend MUST refuse even though the path exists.
        let parent = TempDir::new().unwrap();
        let inside_root = parent.path().join("inside");
        fs::create_dir_all(&inside_root).unwrap();
        let backend = FileSystemSpillBackend::new(&inside_root);

        let outside = parent.path().join("outside.txt");
        fs::write(&outside, b"secret").unwrap();

        let err = backend
            .load(outside.to_str().unwrap())
            .expect_err("must refuse paths outside backend root");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn filesystem_backend_load_missing_file_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        // Canonicalize fails for non-existent paths → NotFound.
        let err = backend
            .load(&dir.path().join("nope.txt").to_string_lossy())
            .expect_err("load of missing file must error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn default_load_impl_returns_unsupported() {
        // A backend that only implements `store` must fail closed on load.
        struct StoreOnly;
        impl SpillBackend for StoreOnly {
            fn store(&self, _k: &str, _b: &[u8]) -> io::Result<String> {
                Ok(String::new())
            }
            // intentionally does NOT override load
        }
        let err = StoreOnly.load("anything").expect_err("default must error");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    // ── SpillRegistry (Phase 12: scheme-based routing) ─────────────────

    #[test]
    fn parse_locator_splits_scheme_and_path() {
        let (s, p) = parse_locator("file:///tmp/x.txt");
        assert_eq!(s, "file");
        assert_eq!(p, "/tmp/x.txt");
    }

    #[test]
    fn parse_locator_bare_path_falls_back_to_default_scheme() {
        let (s, p) = parse_locator("/tmp/legacy.txt");
        assert_eq!(s, DEFAULT_SCHEME);
        assert_eq!(p, "/tmp/legacy.txt");
    }

    #[test]
    fn registry_resolves_registered_scheme() {
        let dir = TempDir::new().unwrap();
        let backend: Arc<dyn SpillBackend> = Arc::new(FileSystemSpillBackend::new(dir.path()));
        let mut reg = SpillRegistry::new();
        reg.register("file", backend);
        assert!(reg.resolve("file").is_some());
        assert!(reg.resolve("s3").is_none());
    }

    #[test]
    fn registry_load_routes_to_scheme_backend() {
        let dir = TempDir::new().unwrap();
        let fs_backend = Arc::new(FileSystemSpillBackend::new(dir.path()));
        let locator = fs_backend.store("k", b"hello from fs").unwrap();

        let mut reg = SpillRegistry::new();
        reg.register("file", fs_backend.clone());

        // Explicit scheme routes correctly.
        let scheme_locator = build_locator("file", &locator);
        let bytes = reg.load(&scheme_locator).expect("load via registry");
        assert_eq!(bytes, b"hello from fs");
    }

    #[test]
    fn registry_legacy_path_falls_back_to_file_scheme() {
        let dir = TempDir::new().unwrap();
        let fs_backend = Arc::new(FileSystemSpillBackend::new(dir.path()));
        let locator = fs_backend.store("k", b"legacy bytes").unwrap();

        let mut reg = SpillRegistry::new();
        reg.register(DEFAULT_SCHEME, fs_backend);

        // A bare path (no scheme) must still resolve via DEFAULT_SCHEME.
        let bytes = reg.load(&locator).expect("legacy path resolves");
        assert_eq!(bytes, b"legacy bytes");
    }

    #[test]
    fn registry_load_unknown_scheme_returns_not_found() {
        let reg = SpillRegistry::new();
        let err = reg
            .load("s3://bucket/key")
            .expect_err("unknown scheme must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn registry_after_process_restart_with_new_backend_instance_resolves() {
        // Simulate process restart: original backend writes bytes, then is
        // dropped; a brand-new backend instance rooted at the same dir
        // resolves the locator via a fresh registry.
        let dir = TempDir::new().unwrap();
        let locator = {
            let first = FileSystemSpillBackend::new(dir.path());
            first.store("k", b"survive restart").unwrap()
        };
        // First backend is dropped here (process "died").

        let reborn: Arc<dyn SpillBackend> = Arc::new(FileSystemSpillBackend::new(dir.path()));
        let mut reg = SpillRegistry::new();
        reg.register(DEFAULT_SCHEME, reborn);

        let bytes = reg.load(&locator).expect("reborn backend resolves");
        assert_eq!(bytes, b"survive restart");
    }

    // ── prune_older_than (Phase 12: GC) ────────────────────────────────

    #[test]
    fn prune_older_than_removes_stale_files() {
        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let path = backend.store("old", b"stale").unwrap();

        // Backdate the file's mtime by 2 hours.
        let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(two_hours_ago))
            .unwrap();

        let removed = backend
            .prune_older_than(Duration::from_secs(3600))
            .expect("prune");
        assert_eq!(removed, 1);
        assert!(!Path::new(&path).exists(), "stale file should be gone");
    }

    #[test]
    fn prune_older_than_keeps_fresh_files() {
        let dir = TempDir::new().unwrap();
        let backend = FileSystemSpillBackend::new(dir.path());
        let path = backend.store("fresh", b"new").unwrap();

        let removed = backend
            .prune_older_than(Duration::from_secs(3600))
            .expect("prune");
        assert_eq!(removed, 0);
        assert!(Path::new(&path).exists(), "fresh file should remain");
    }

    #[test]
    fn prune_older_than_missing_root_is_noop() {
        let parent = TempDir::new().unwrap();
        let never_created = parent.path().join("never");
        let backend = FileSystemSpillBackend::new(&never_created);
        // root was never created — GC must not error.
        let removed = backend
            .prune_older_than(Duration::from_secs(1))
            .expect("prune on missing root is ok");
        assert_eq!(removed, 0);
    }
}
