//! Sweep orphaned agent worktrees.
//!
//! An "orphaned" worktree is one present on disk under `.agent-worktrees/`
//! but either (a) not tracked in [`WorktreeRegistry`], or (b) whose
//! heartbeat is older than `ttl`.
//!
//! Sweep is best-effort: failures are logged and swallowed so a broken
//! worktree never blocks a new spawn.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use tracing::{debug, warn};

use super::worktree_registry::{WorktreeEntry, WorktreeRegistry};

/// Default heartbeat TTL: entries older than this are considered dead.
pub const DEFAULT_STALE_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Default, Clone)]
pub struct SweepReport {
    pub scanned: usize,
    pub removed: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    /// Registry rows whose heartbeats expired and were dropped during this
    /// sweep. Independent of `removed` (which is filesystem dirs): a row
    /// can be pruned without a corresponding directory existing, and
    /// vice versa.
    pub pruned_registry_rows: Vec<String>,
}

impl SweepReport {
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }
}

/// Sweep orphaned worktrees under `base` (typically `<repo>/.agent-worktrees`).
///
/// - `base` is the directory containing per-run worktree subdirs.
/// - `registry` tracks live agents; entries older than `ttl` are considered stale.
/// - Returns a report; errors during individual removals do not abort the sweep.
pub fn sweep_orphaned_worktrees(
    base: &Path,
    registry: &WorktreeRegistry,
    ttl: Duration,
) -> SweepReport {
    let mut report = SweepReport::default();

    if !base.exists() {
        return report;
    }

    let entries = match std::fs::read_dir(base) {
        Ok(e) => e,
        Err(e) => {
            warn!("sweep: read_dir({}) failed: {}", base.display(), e);
            return report;
        }
    };

    let now = SystemTime::now();
    let live: std::collections::HashMap<String, WorktreeEntry> = registry
        .snapshot()
        .into_iter()
        .map(|e| (e.run_id.clone(), e))
        .collect();

    for dirent in entries.flatten() {
        let path = dirent.path();
        // Skip non-dirs and the registry file itself.
        if !path.is_dir() {
            continue;
        }
        let run_id = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        report.scanned += 1;

        let should_remove = match live.get(&run_id) {
            None => true, // no registry entry → orphan
            Some(entry) => now
                .duration_since(entry.last_heartbeat)
                .map(|age| age > ttl)
                .unwrap_or(false),
        };

        if !should_remove {
            continue;
        }

        // C-ORCH-2: TOCTOU mitigation. The `live` map was a snapshot
        // taken before scanning. Re-check liveness against a fresh
        // snapshot **right before** removing — another process may have
        // registered or heartbeated this run_id since we listed the dir.
        let fresh: std::collections::HashMap<String, WorktreeEntry> = registry
            .snapshot()
            .into_iter()
            .map(|e| (e.run_id.clone(), e))
            .collect();
        let still_orphan = match fresh.get(&run_id) {
            None => true,
            Some(entry) => SystemTime::now()
                .duration_since(entry.last_heartbeat)
                .map(|age| age > ttl)
                .unwrap_or(false),
        };
        if !still_orphan {
            debug!(
                "sweep: {} became live between scan and remove; skipping",
                path.display()
            );
            continue;
        }

        match remove_worktree(&path) {
            Ok(()) => {
                debug!("sweep: removed {}", path.display());
                // Best-effort registry cleanup too.
                let _ = registry.unregister(&run_id);
                report.removed.push(path);
            }
            Err(e) => {
                warn!("sweep: failed to remove {}: {}", path.display(), e);
                report.failed.push((path, e));
            }
        }
    }

    // Prune stale git worktree admin dirs.
    if let Err(e) = git_worktree_prune(base) {
        debug!("sweep: git worktree prune warning: {}", e);
    }

    // Drop dead rows from the registry. Without this the JSON file
    // grows forever — `register` adds rows, `unregister` removes them
    // on graceful exit, but a crashed agent never gets a chance to
    // unregister. `prune_stale` uses the same TTL as the sweep so the
    // two reclamation paths agree on what "dead" means, and tombstones
    // the run_ids so a concurrent process can't resurrect them.
    match registry.prune_stale(ttl) {
        Ok(ids) if !ids.is_empty() => {
            debug!("sweep: pruned {} stale registry rows", ids.len());
            report.pruned_registry_rows = ids;
        }
        Ok(_) => {}
        Err(e) => debug!("sweep: prune_stale warning: {}", e),
    }

    report
}

fn remove_worktree(path: &Path) -> Result<(), String> {
    // Always run `git worktree remove` from inside the parent repo. When
    // launched from a different cwd (or from a sweep triggered far away)
    // git can't find the repo and silently falls back to rm -rf, leaving
    // the parent repo's `.git/worktrees/<name>` admin dir behind.
    let repo_root = path
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| Path::new("."));

    let git_result = Command::new("git")
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(path)
        .current_dir(repo_root)
        .output();

    match git_result {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Fallback: not a worktree → just rm -rf, then prune the admin dir.
            if stderr.contains("not a working tree") || stderr.contains("is not a working tree") {
                std::fs::remove_dir_all(path).map_err(|e| format!("rm -rf: {e}"))?;
                // Best-effort: ask git to drop any admin record that may still
                // reference this path (no-op if nothing matches).
                let _ = Command::new("git")
                    .arg("worktree")
                    .arg("prune")
                    .current_dir(repo_root)
                    .output();
                Ok(())
            } else {
                Err(format!("git worktree remove: {}", stderr.trim()))
            }
        }
        Err(e) => Err(format!("git spawn: {e}")),
    }
}

fn git_worktree_prune(cwd: &Path) -> Result<(), String> {
    // Run prune from the parent repo; `cwd` is .agent-worktrees, so go up.
    let repo_root = cwd.parent().unwrap_or(cwd);
    let out = Command::new("git")
        .arg("worktree")
        .arg("prune")
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("git spawn: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn init_git_repo(path: &Path) {
        let out = Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(path)
            .output()
            .expect("git init");
        assert!(out.status.success(), "git init failed");
        // minimal config + commit so worktree add works
        Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(path)
            .output()
            .unwrap();
        fs::write(path.join("README"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(path)
            .output()
            .unwrap();
    }

    #[test]
    fn sweep_on_missing_base_is_noop() {
        let tmp = tempdir().unwrap();
        let base = tmp.path().join("nonexistent");
        let reg = WorktreeRegistry::load_or_init(tmp.path()).unwrap();
        let r = sweep_orphaned_worktrees(&base, &reg, DEFAULT_STALE_TTL);
        assert_eq!(r.scanned, 0);
        assert!(r.removed.is_empty());
    }

    #[test]
    fn sweep_removes_untracked_dir() {
        let tmp = tempdir().unwrap();
        init_git_repo(tmp.path());
        let base = tmp.path().join(".agent-worktrees");
        fs::create_dir_all(&base).unwrap();
        // Create a plain dir (not a real git worktree) → sweep should rm -rf it.
        let orphan = base.join("orphan-run-id");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("data"), "x").unwrap();

        let reg = WorktreeRegistry::load_or_init(&base).unwrap();
        let r = sweep_orphaned_worktrees(&base, &reg, DEFAULT_STALE_TTL);
        assert_eq!(r.scanned, 1);
        assert_eq!(r.removed.len(), 1);
        assert!(!orphan.exists());
    }

    #[test]
    fn sweep_keeps_live_registered() {
        let tmp = tempdir().unwrap();
        init_git_repo(tmp.path());
        let base = tmp.path().join(".agent-worktrees");
        fs::create_dir_all(&base).unwrap();
        let live = base.join("live-run");
        fs::create_dir_all(&live).unwrap();

        let reg = WorktreeRegistry::load_or_init(&base).unwrap();
        reg.register(WorktreeEntry {
            run_id: "live-run".into(),
            worktree_path: live.clone(),
            pid: std::process::id(),
            started_at: SystemTime::now(),
            last_heartbeat: SystemTime::now(),
        })
        .unwrap();

        let r = sweep_orphaned_worktrees(&base, &reg, DEFAULT_STALE_TTL);
        assert_eq!(r.scanned, 1);
        assert!(r.removed.is_empty());
        assert!(live.exists());
    }

    #[test]
    fn sweep_removes_stale_heartbeat() {
        let tmp = tempdir().unwrap();
        init_git_repo(tmp.path());
        let base = tmp.path().join(".agent-worktrees");
        fs::create_dir_all(&base).unwrap();
        let stale = base.join("stale-run");
        fs::create_dir_all(&stale).unwrap();

        let reg = WorktreeRegistry::load_or_init(&base).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        reg.register(WorktreeEntry {
            run_id: "stale-run".into(),
            worktree_path: stale.clone(),
            pid: 999999,
            started_at: old,
            last_heartbeat: old,
        })
        .unwrap();

        let r = sweep_orphaned_worktrees(&base, &reg, Duration::from_secs(60));
        assert_eq!(r.removed.len(), 1);
        assert!(!stale.exists());
        // registry entry should be gone too
        assert!(reg.snapshot().iter().all(|e| e.run_id != "stale-run"));
    }
}
