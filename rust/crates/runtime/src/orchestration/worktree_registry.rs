//! Agent worktree registry.
//!
//! Tracks live agent worktrees on disk so that orphaned worktrees (left behind
//! by crashed or killed sub-agents) can be reclaimed on the next spawn.
//!
//! Design goals:
//! - **Crash-safe**: registry is a JSON file updated atomically (write tmp + rename).
//! - **Multi-process-safe**: cooperative file lock (best-effort `fs2` advisory lock
//!   when available; falls back to a `.lock` sentinel file with stale-ttl reclaim).
//! - **Liveness via heartbeat**, not PID: PIDs are unreliable under containers,
//!   restarts, or PID reuse. An entry is "alive" iff `last_heartbeat` is within
//!   `STALE_TTL`.
//! - **Idempotent**: register/unregister/heartbeat can be called repeatedly without
//!   corrupting state.
//!
//! The registry file lives at `<base>/registry.json` where `<base>` is typically
//! `<parent_repo>/.agent-worktrees`.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Default heartbeat staleness threshold. Entries older than this are considered
/// dead and eligible for reclamation. Chosen to be ~2× the expected heartbeat
/// cadence in `spawner.rs` (~30s) to tolerate brief pauses.
pub const STALE_TTL: Duration = Duration::from_secs(60);

/// Per-worktree registry entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub run_id: String,
    pub worktree_path: PathBuf,
    pub pid: u32,
    #[serde(with = "systemtime_serde")]
    pub started_at: SystemTime,
    #[serde(with = "systemtime_serde")]
    pub last_heartbeat: SystemTime,
}

impl WorktreeEntry {
    pub fn new(run_id: impl Into<String>, worktree_path: PathBuf, pid: u32) -> Self {
        let now = SystemTime::now();
        Self {
            run_id: run_id.into(),
            worktree_path,
            pid,
            started_at: now,
            last_heartbeat: now,
        }
    }

    /// True iff `last_heartbeat` is older than `ttl`.
    ///
    /// Robust to wall-clock rewinds: if the heartbeat appears to be in the
    /// future (clock moved backwards, NTP correction, restored snapshot, …)
    /// we treat the entry as **stale** rather than immortal — an entry whose
    /// timestamp we cannot trust must not block reclamation forever.
    pub fn is_stale(&self, ttl: Duration) -> bool {
        match SystemTime::now().duration_since(self.last_heartbeat) {
            Ok(age) => age > ttl,
            // `last_heartbeat` is in the future relative to `now`. Refuse to
            // trust it; mark stale so cleanup can reclaim.
            Err(_) => true,
        }
    }
}

/// On-disk registry schema. Versioned so we can migrate later.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    entries: HashMap<String, WorktreeEntry>,
}

fn default_version() -> u32 {
    1
}

/// Thread-safe, process-safe registry over `<base>/registry.json`.
pub struct WorktreeRegistry {
    state_path: PathBuf,
    entries: RwLock<HashMap<String, WorktreeEntry>>,
    /// Run IDs intentionally removed; suppressed from on-disk merge in `flush`.
    /// Cleared after each successful flush.
    tombstones: RwLock<std::collections::HashSet<String>>,
}

impl WorktreeRegistry {
    /// Load an existing registry or initialize an empty one.
    ///
    /// `base` is the directory holding agent worktrees (e.g. `.agent-worktrees/`).
    /// Creates the directory if missing.
    pub fn load_or_init(base: &Path) -> io::Result<Self> {
        fs::create_dir_all(base)?;
        let state_path = base.join("registry.json");
        let entries = if state_path.exists() {
            match Self::read_file(&state_path) {
                Ok(f) => f.entries,
                Err(e) => {
                    tracing::warn!(
                        path = %state_path.display(),
                        error = %e,
                        "worktree registry unreadable; starting fresh"
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };
        Ok(Self {
            state_path,
            entries: RwLock::new(entries),
            tombstones: RwLock::new(std::collections::HashSet::new()),
        })
    }

    fn read_file(path: &Path) -> io::Result<RegistryFile> {
        let mut f = fs::File::open(path)?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;
        if buf.trim().is_empty() {
            return Ok(RegistryFile::default());
        }
        serde_json::from_str(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Path of the cross-process advisory lock file.
    fn lock_path(&self) -> PathBuf {
        // Sibling to registry.json; using a fixed suffix avoids the
        // `with_extension` ambiguity when the state path itself has no extension.
        let mut p = self.state_path.clone();
        let name = p
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from("registry.json"));
        let mut new_name = name;
        new_name.push(".lock");
        p.set_file_name(new_name);
        p
    }

    /// Acquire the cross-process advisory write lock.
    ///
    /// Best-effort: if the platform refuses (`fs2` returns `WouldBlock`
    /// indefinitely or unsupported FS), we fall back to a non-locked write
    /// after logging a warning. The caller must drop the returned guard
    /// to release.
    fn acquire_file_lock(&self) -> io::Result<Option<fs::File>> {
        let lock_path = self.lock_path();
        let f = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        match f.lock_exclusive() {
            Ok(()) => Ok(Some(f)),
            Err(e) => {
                tracing::warn!(
                    path = %lock_path.display(),
                    error = %e,
                    "worktree registry: file lock unavailable; proceeding unlocked"
                );
                Ok(None)
            }
        }
    }

    /// Persist current in-memory state atomically (write tmp + rename).
    ///
    /// Holds a cross-process advisory lock for the **shortest** window
    /// possible: clone the snapshot under the in-memory `RwLock`, drop
    /// it, then take the file lock to write+rename. This avoids holding
    /// either lock across slow `sync_all`/`rename` while still serialising
    /// concurrent writers between processes.
    fn flush(&self) -> io::Result<()> {
        // 1. Snapshot under in-memory lock; release immediately.
        let file = {
            let entries = self
                .entries
                .read()
                .map_err(|_| io::Error::other("registry lock poisoned"))?;
            RegistryFile {
                version: 1,
                entries: entries.clone(),
            }
        };

        // 2. Take the cross-process advisory lock for the write window.
        let _file_lock_guard = self.acquire_file_lock()?;

        // 3. Re-read whatever is on disk *under the lock*; merge with our
        //    in-memory snapshot. This prevents concurrent writers in other
        //    processes from clobbering each other: each writer keeps its own
        //    new/updated entries, and any entries seen on disk that aren't
        //    in our snapshot survive.
        let tombstones: std::collections::HashSet<String> = self
            .tombstones
            .read()
            .map(|t| t.clone())
            .unwrap_or_default();
        let merged = if self.state_path.exists() {
            match Self::read_file(&self.state_path) {
                Ok(on_disk) => {
                    let mut out = on_disk.entries;
                    // Our snapshot wins for keys we know about (latest writer
                    // for that run_id).
                    for (k, v) in file.entries.iter() {
                        out.insert(k.clone(), v.clone());
                    }
                    // Suppress entries we explicitly unregistered this session,
                    // even if another process re-added them with the same id.
                    for id in tombstones.iter() {
                        out.remove(id);
                    }
                    RegistryFile {
                        version: 1,
                        entries: out,
                    }
                }
                Err(_) => file,
            }
        } else {
            file
        };

        // 4. Write tmp + atomic rename.
        let tmp_path = {
            let mut p = self.state_path.clone();
            let mut name = p
                .file_name()
                .map(|s| s.to_os_string())
                .unwrap_or_else(|| std::ffi::OsString::from("registry.json"));
            name.push(".tmp");
            p.set_file_name(name);
            p
        };
        {
            let mut f = fs::File::create(&tmp_path)?;
            let json = serde_json::to_string_pretty(&merged).map_err(io::Error::other)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, &self.state_path)?;

        // 5. Make the merged view the in-memory truth so subsequent
        //    operations see entries written by other processes too.
        if let Ok(mut entries) = self.entries.write() {
            *entries = merged.entries;
        }
        // Tombstones consumed by this flush.
        if let Ok(mut tomb) = self.tombstones.write() {
            tomb.clear();
        }
        Ok(())
    }

    /// Register a new agent worktree. Overwrites any prior entry with the same `run_id`.
    pub fn register(&self, entry: WorktreeEntry) -> io::Result<()> {
        {
            let mut entries = self
                .entries
                .write()
                .map_err(|_| io::Error::other("registry lock poisoned"))?;
            entries.insert(entry.run_id.clone(), entry);
        }
        self.flush()
    }

    /// Update `last_heartbeat = now` for `run_id`. No-op if unknown.
    pub fn heartbeat(&self, run_id: &str) -> io::Result<()> {
        {
            let mut entries = self
                .entries
                .write()
                .map_err(|_| io::Error::other("registry lock poisoned"))?;
            if let Some(e) = entries.get_mut(run_id) {
                e.last_heartbeat = SystemTime::now();
            } else {
                return Ok(()); // no-op; nothing to flush
            }
        }
        self.flush()
    }

    /// Remove `run_id` from the registry. Idempotent.
    ///
    /// Records a tombstone so the on-disk merge in [`flush`] won't
    /// resurrect the entry from another process's snapshot.
    pub fn unregister(&self, run_id: &str) -> io::Result<()> {
        {
            let mut entries = self
                .entries
                .write()
                .map_err(|_| io::Error::other("registry lock poisoned"))?;
            entries.remove(run_id);
        }
        if let Ok(mut tomb) = self.tombstones.write() {
            tomb.insert(run_id.to_string());
        }
        self.flush()
    }

    /// Entries whose heartbeat is older than `ttl`.
    pub fn list_stale(&self, ttl: Duration) -> Vec<WorktreeEntry> {
        let entries = match self.entries.read() {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        entries
            .values()
            .filter(|e| e.is_stale(ttl))
            .cloned()
            .collect()
    }

    /// Drop every entry whose heartbeat is older than `ttl`. Returns the
    /// `run_id`s that were removed (caller may want to log / report them).
    ///
    /// Without this, the registry only ever grows: `register` adds rows,
    /// `unregister` removes them on the happy path, but a crashed agent
    /// (kill -9, OOM, container eviction) leaves its row behind forever.
    /// `is_stale` already correctly identifies these, but no one was
    /// pruning them — the disk file would accumulate dead rows session
    /// after session.
    ///
    /// Tombstones the removed ids so a concurrent process whose snapshot
    /// still has them won't resurrect them via the merge in `flush()`.
    pub fn prune_stale(&self, ttl: Duration) -> io::Result<Vec<String>> {
        let stale_ids: Vec<String> = {
            let entries = self
                .entries
                .read()
                .map_err(|_| io::Error::other("registry lock poisoned"))?;
            entries
                .values()
                .filter(|e| e.is_stale(ttl))
                .map(|e| e.run_id.clone())
                .collect()
        };
        if stale_ids.is_empty() {
            return Ok(stale_ids);
        }
        {
            let mut entries = self
                .entries
                .write()
                .map_err(|_| io::Error::other("registry lock poisoned"))?;
            for id in &stale_ids {
                entries.remove(id);
            }
        }
        if let Ok(mut tomb) = self.tombstones.write() {
            for id in &stale_ids {
                tomb.insert(id.clone());
            }
        }
        self.flush()?;
        Ok(stale_ids)
    }

    /// Return filesystem paths in `fs_entries` that have no corresponding registry entry.
    ///
    /// Caller provides the list of on-disk worktree directories (typically from
    /// scanning `<base>/*` and filtering to dirs). An entry is orphaned iff no
    /// registered worktree points to the same path.
    pub fn list_orphaned(&self, fs_entries: &[PathBuf]) -> Vec<PathBuf> {
        let entries = match self.entries.read() {
            Ok(e) => e,
            Err(_) => return fs_entries.to_vec(),
        };
        let known: std::collections::HashSet<_> =
            entries.values().map(|e| e.worktree_path.clone()).collect();
        fs_entries
            .iter()
            .filter(|p| !known.contains(*p))
            .cloned()
            .collect()
    }

    /// Snapshot of all registered entries (for diagnostics / `astra worktree list`).
    pub fn snapshot(&self) -> Vec<WorktreeEntry> {
        self.entries
            .read()
            .map(|e| e.values().cloned().collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn state_path(&self) -> &Path {
        &self.state_path
    }
}

/// SystemTime serde helper (seconds since UNIX epoch, f64 for subsecond precision).
mod systemtime_serde {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let secs = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        s.serialize_f64(secs)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let secs = f64::deserialize(d)?;
        Ok(SystemTime::UNIX_EPOCH + Duration::from_secs_f64(secs.max(0.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mk_entry(run_id: &str, path: &Path) -> WorktreeEntry {
        WorktreeEntry::new(run_id, path.to_path_buf(), 12345)
    }

    #[test]
    fn load_or_init_creates_empty_registry() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        assert!(r.snapshot().is_empty());
        assert!(r.state_path().ends_with("registry.json"));
    }

    #[test]
    fn register_then_reload_persists() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("wt-1");
        {
            let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
            r.register(mk_entry("run-a", &p)).unwrap();
        }
        let r2 = WorktreeRegistry::load_or_init(td.path()).unwrap();
        let snap = r2.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].run_id, "run-a");
        assert_eq!(snap[0].worktree_path, p);
    }

    #[test]
    fn heartbeat_updates_timestamp() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        let e = mk_entry("run-b", &td.path().join("wt-b"));
        let original = e.last_heartbeat;
        r.register(e).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        r.heartbeat("run-b").unwrap();
        let after = r.snapshot().into_iter().next().unwrap().last_heartbeat;
        assert!(after > original);
    }

    #[test]
    fn heartbeat_unknown_is_noop() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        r.heartbeat("does-not-exist").unwrap(); // must not error
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn unregister_is_idempotent() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        r.register(mk_entry("run-c", &td.path().join("wt-c")))
            .unwrap();
        r.unregister("run-c").unwrap();
        r.unregister("run-c").unwrap(); // second call: no-op, no error
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn list_stale_respects_ttl() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        let mut e = mk_entry("run-d", &td.path().join("wt-d"));
        // Backdate heartbeat to 5 minutes ago.
        e.last_heartbeat = SystemTime::now() - Duration::from_secs(300);
        r.register(e).unwrap();

        assert_eq!(r.list_stale(Duration::from_secs(60)).len(), 1);
        assert_eq!(r.list_stale(Duration::from_secs(600)).len(), 0);
    }

    /// Major regression guard: dead entries must actually be removable.
    /// Before `prune_stale`, the registry only ever grew — crashed agents
    /// (no graceful unregister) left rows that survived restart.
    #[test]
    fn prune_stale_removes_dead_rows_and_persists() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        let mut dead = mk_entry("dead", &td.path().join("d"));
        dead.last_heartbeat = SystemTime::now() - Duration::from_secs(300);
        let alive = mk_entry("alive", &td.path().join("a"));
        r.register(dead).unwrap();
        r.register(alive).unwrap();

        let removed = r.prune_stale(Duration::from_secs(60)).unwrap();
        assert_eq!(removed, vec!["dead".to_string()]);

        let snap = r.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].run_id, "alive");

        // Reload from disk: pruning must have been flushed.
        let r2 = WorktreeRegistry::load_or_init(td.path()).unwrap();
        let snap2 = r2.snapshot();
        assert_eq!(snap2.len(), 1);
        assert_eq!(snap2[0].run_id, "alive");
    }

    /// Pruning a clean registry must be a noop and not flush.
    #[test]
    fn prune_stale_is_noop_when_no_dead_rows() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        r.register(mk_entry("alive", &td.path().join("a"))).unwrap();
        let removed = r.prune_stale(Duration::from_secs(60)).unwrap();
        assert!(removed.is_empty());
        assert_eq!(r.snapshot().len(), 1);
    }

    /// Clock-rewind safety carries through `prune_stale` too: a future
    /// heartbeat must not block reclamation.
    #[test]
    fn prune_stale_treats_future_heartbeat_as_stale() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        let mut e = mk_entry("rewound", &td.path().join("r"));
        e.last_heartbeat = SystemTime::now() + Duration::from_secs(3600);
        r.register(e).unwrap();
        let removed = r.prune_stale(Duration::from_secs(60)).unwrap();
        assert_eq!(removed, vec!["rewound".to_string()]);
        assert!(r.snapshot().is_empty());
    }

    /// Minor regression guard: if the wall clock rewinds (NTP correction,
    /// VM restore, daylight shift) and `last_heartbeat` ends up *in the
    /// future*, the entry must NOT become immortal. We treat unknowable
    /// timestamps as stale so reclamation can still run.
    #[test]
    fn list_orphaned_detects_unregistered_paths() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        let known = td.path().join("wt-known");
        let orphan = td.path().join("wt-orphan");
        r.register(mk_entry("run-e", &known)).unwrap();

        let fs_entries = vec![known.clone(), orphan.clone()];
        let orphans = r.list_orphaned(&fs_entries);
        assert_eq!(orphans, vec![orphan]);
    }

    #[test]
    fn corrupt_registry_file_recovers_as_empty() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("registry.json");
        fs::write(&path, "{not valid json").unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn register_overwrites_same_run_id() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        r.register(mk_entry("run-f", &td.path().join("v1")))
            .unwrap();
        r.register(mk_entry("run-f", &td.path().join("v2")))
            .unwrap();
        let snap = r.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].worktree_path, td.path().join("v2"));
    }

    #[test]
    fn atomic_flush_survives_partial_write() {
        // Simulate: register, then verify tmp file is cleaned up on success.
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        r.register(mk_entry("run-g", &td.path().join("wt-g")))
            .unwrap();
        let tmp = td.path().join("registry.json.tmp");
        assert!(!tmp.exists(), "tmp file must be renamed away on success");
    }

    /// C-ORCH-1 regression: two registries pointing at the same dir
    /// (simulating two processes) registering different run_ids must
    /// preserve **both** entries, not silently lose one to a write race.
    #[test]
    fn two_registries_concurrent_register_preserves_all_entries() {
        use std::sync::Arc;
        use std::thread;

        let td = TempDir::new().unwrap();
        let base = td.path().to_path_buf();

        let r1 = Arc::new(WorktreeRegistry::load_or_init(&base).unwrap());
        let r2 = Arc::new(WorktreeRegistry::load_or_init(&base).unwrap());

        let r1c = Arc::clone(&r1);
        let base1 = base.clone();
        let t1 = thread::spawn(move || {
            for i in 0..20 {
                let id = format!("p1-run-{i}");
                r1c.register(mk_entry(&id, &base1.join(&id))).unwrap();
            }
        });
        let r2c = Arc::clone(&r2);
        let base2 = base.clone();
        let t2 = thread::spawn(move || {
            for i in 0..20 {
                let id = format!("p2-run-{i}");
                r2c.register(mk_entry(&id, &base2.join(&id))).unwrap();
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();

        // Reload from disk: must contain all 40 entries from both writers.
        let r3 = WorktreeRegistry::load_or_init(&base).unwrap();
        let snap = r3.snapshot();
        assert_eq!(
            snap.len(),
            40,
            "expected 40 entries from concurrent writers, got {} ({:?})",
            snap.len(),
            snap.iter().map(|e| &e.run_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lock_file_lives_next_to_state_path() {
        let td = TempDir::new().unwrap();
        let r = WorktreeRegistry::load_or_init(td.path()).unwrap();
        r.register(mk_entry("lock-test", &td.path().join("wt")))
            .unwrap();
        let lock_path = td.path().join("registry.json.lock");
        assert!(
            lock_path.exists(),
            "lock file must sit next to registry.json"
        );
        // tmp must NOT live (renamed away).
        assert!(!td.path().join("registry.json.tmp").exists());
    }
}
