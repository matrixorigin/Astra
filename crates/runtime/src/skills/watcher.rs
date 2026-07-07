//! File-system watcher for skill hot-reload.
//!
//! Watches skill search paths (`.astra/skills/`, `.claude/skills/`,
//! `.agent/skills/`, and HOME skill directories) for changes and triggers a registry re-discover when
//! SKILL.md or manifest.yaml files are created, modified, or deleted.
//!
//! ## fd budget
//!
//! On macOS the `notify` crate is built with `macos_kqueue` (FSEvents has
//! coarse-grained debouncing that delays skill hot-reload). kqueue takes one
//! file descriptor per watched path and recursive watches enumerate every
//! file in the subtree. Naively watching `~/.claude/skills/` with
//! `RecursiveMode::Recursive` therefore opens an fd for every file under the
//! tree — including `node_modules/` belonging to skills that bundle a
//! Node-based runtime. A single skill like `gstack` (puppeteer + huggingface
//! transformers) carries ~16k files and exhausts the macOS default soft
//! limit (256), causing seemingly-unrelated failures across the process
//! (LLM HTTP sockets fail to open, credentials file lock fails, etc.).
//!
//! To stay within budget we mirror the registry's discovery semantics from
//! [`astra_skills::loader::discover_skills_in_dir`]:
//!   1. For each search-path root, enumerate immediate children only via
//!      `read_dir` and accept ones whose direct child is `SKILL.md`. This
//!      matches the canonical Agent Skills layout (`{root}/{skill_name}/SKILL.md`)
//!      and avoids any descent into nested `node_modules` / `.git` / etc.
//!   2. Watch each search-path root with `NonRecursive` to see new skill
//!      directories appearing alongside existing ones.
//!   3. Watch each discovered skill directory with `NonRecursive` to see
//!      edits to its `SKILL.md` / `manifest.yaml`. Assets nested below
//!      the skill root are not part of the registry contract.
//!
//! Newly created skills are picked up on the next debounced refresh because
//! the root watch fires for the new child directory, and the registry's
//! `discover_all` then loads them. Subsequent edits to those skills'
//! `SKILL.md` / `manifest.yaml` will not, however, trigger hot-reload until
//! the process is restarted — only skill directories present at startup get
//! a per-skill watch (acceptable trade-off vs. blowing the fd budget).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use super::registry::UnifiedSkillRegistry;

/// Default debounce interval for skill file changes.
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(500);

/// Cap on the number of skill directories we will watch from a single root.
/// A pathological tree with thousands of `SKILL.md` files would still blow
/// the fd budget; refuse rather than corrupt the rest of the process.
/// 256 is comfortably above any realistic deployment (the Agent Skills
/// ecosystem rarely tops a few dozen skills per root) while staying well
/// under typical raised fd limits.
///
/// When the cap fires, skills past the cap are **still loaded** by the
/// registry (the cap only governs how many per-skill watches we install)
/// but their `SKILL.md` / `manifest.yaml` edits will not trigger hot-reload
/// until the process is restarted. New skill directories created after
/// startup are still detected via the root's non-recursive watch.
const MAX_WATCHED_SKILLS_PER_ROOT: usize = 256;

/// Handle returned by [`start_watching`]. Drop to stop the watcher.
pub struct SkillWatcherHandle {
    _watcher: notify::RecommendedWatcher,
    _task: tokio::task::JoinHandle<()>,
}

impl SkillWatcherHandle {
    /// Stop watching (the watcher and background task are dropped).
    pub fn stop(self) {
        drop(self);
    }
}

/// Start watching skill directories for changes.
///
/// When a relevant file change is detected (SKILL.md, manifest.yaml, or
/// directory structure change), the registry is refreshed with debouncing.
///
/// Returns `None` if no watchable directories exist or the watcher fails to start.
pub fn start_watching(
    registry: Arc<UnifiedSkillRegistry>,
    watch_paths: Vec<PathBuf>,
) -> Option<SkillWatcherHandle> {
    let existing_paths: Vec<PathBuf> = watch_paths.into_iter().filter(|p| p.exists()).collect();
    if existing_paths.is_empty() {
        return None;
    }

    let (tx, rx) = mpsc::unbounded_channel::<()>();

    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if is_skill_relevant_event(&event) {
                    let _ = tx.send(());
                }
            }
        })
        .ok()?;

    // Each kqueue watch costs one fd. Two-tier scheme: one fd for the root
    // (catches new-skill creates) plus one fd per existing skill directory
    // (catches SKILL.md/manifest edits). With the cap, this stays bounded
    // regardless of what's underneath the search path.
    let mut watched: HashSet<PathBuf> = HashSet::new();
    for root in &existing_paths {
        if watched.insert(root.clone()) && watcher.watch(root, RecursiveMode::NonRecursive).is_err()
        {
            eprintln!("  ⚠ Failed to watch skill root: {}", root.display());
        }

        for skill_dir in discover_skill_dirs(root, MAX_WATCHED_SKILLS_PER_ROOT) {
            if watched.insert(skill_dir.clone())
                && watcher
                    .watch(&skill_dir, RecursiveMode::NonRecursive)
                    .is_err()
            {
                eprintln!("  ⚠ Failed to watch skill dir: {}", skill_dir.display());
            }
        }
    }

    let task = tokio::spawn(debounced_refresh_loop(registry, rx));

    Some(SkillWatcherHandle {
        _watcher: watcher,
        _task: task,
    })
}

/// Enumerate immediate child directories of `root` that contain a `SKILL.md`.
///
/// This mirrors [`astra_skills::loader::discover_skills_in_dir`] semantics so
/// the watcher and the registry agree on what counts as a skill:
/// - non-recursive (depth = 1)
/// - canonicalize each candidate; reject paths that escape the search root
///   via symlink, and dedup repeat canonical paths
///
/// Capped at `cap` entries to bound fd usage on pathological trees.
fn discover_skill_dirs(root: &Path, cap: usize) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let canonical_root = match std::fs::canonicalize(root) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();

    for entry in entries.flatten() {
        let path = entry.path();
        // Match loader's `path.is_dir()` (follows symlinks). Files (including
        // a top-level SKILL.md) are not skill containers.
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        // Canonicalize via SKILL.md so that a symlinked skill dir whose
        // SKILL.md ultimately resolves outside `canonical_root` is rejected
        // — the loader does this for security and we follow suit so that
        // anything we watch is something the registry would actually load.
        let Ok(canonical_md) = std::fs::canonicalize(&skill_md) else {
            continue;
        };
        if !canonical_md.starts_with(&canonical_root) {
            continue;
        }
        // Watch the directory containing SKILL.md, in canonical form. Using
        // the canonical parent (rather than the symlink path `path`) ensures
        // dedup works when two symlinks point at the same skill, and that
        // notify watches the real inode rather than a symlink that may be
        // recreated.
        let Some(canonical_dir) = canonical_md.parent() else {
            continue;
        };
        let canonical_dir = canonical_dir.to_path_buf();
        if !seen_canonical.insert(canonical_dir.clone()) {
            continue;
        }
        out.push(canonical_dir);
        if out.len() >= cap {
            eprintln!(
                "  ⚠ Skill discovery cap reached at {} dirs under {}; remaining skills will not be watched (hot-reload disabled for them).",
                cap,
                root.display()
            );
            break;
        }
    }
    out
}

/// Returns true if the filesystem event is relevant to skill files.
fn is_skill_relevant_event(event: &notify::Event) -> bool {
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
        _ => return false,
    }

    event.paths.iter().any(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        name == "SKILL.md"
            || name == "manifest.yaml"
            || name == "manifest.yml"
            // Directory-level changes (new skill dirs, renames)
            || matches!(event.kind, EventKind::Create(notify::event::CreateKind::Folder)
                | EventKind::Remove(notify::event::RemoveKind::Folder))
    })
}

/// Background loop that debounces rapid file changes and refreshes the registry.
async fn debounced_refresh_loop(
    registry: Arc<UnifiedSkillRegistry>,
    mut rx: mpsc::UnboundedReceiver<()>,
) {
    loop {
        // Wait for the first change notification
        if rx.recv().await.is_none() {
            break; // Sender dropped — watcher stopped
        }

        // Debounce: drain any additional events arriving within the interval
        tokio::time::sleep(DEBOUNCE_INTERVAL).await;
        while rx.try_recv().is_ok() {}

        // Re-discover skills
        match registry.discover_all().await {
            Ok(names) => {
                eprintln!("  🔄 Skills reloaded ({} discovered)", names.len());
            }
            Err(e) => {
                eprintln!("  ⚠ Skill reload failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn watcher_returns_none_for_nonexistent_paths() {
        let registry = Arc::new(UnifiedSkillRegistry::new());
        let handle = start_watching(
            registry,
            vec![PathBuf::from("/nonexistent/path/1234567890")],
        );
        assert!(handle.is_none());
    }

    #[tokio::test]
    async fn watcher_starts_for_existing_paths() {
        let dir = TempDir::new().unwrap();
        let registry = Arc::new(UnifiedSkillRegistry::new());
        let handle = start_watching(registry, vec![dir.path().to_path_buf()]);
        assert!(handle.is_some());
    }

    #[test]
    fn skill_relevant_event_filters_correctly() {
        let skill_event = notify::Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("/skills/review/SKILL.md")],
            attrs: Default::default(),
        };
        assert!(is_skill_relevant_event(&skill_event));

        let irrelevant_event = notify::Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("/src/main.rs")],
            attrs: Default::default(),
        };
        assert!(!is_skill_relevant_event(&irrelevant_event));

        let access_event = notify::Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![PathBuf::from("/skills/review/SKILL.md")],
            attrs: Default::default(),
        };
        assert!(!is_skill_relevant_event(&access_event));
    }

    /// Discovery walks one level deep and returns each `{root}/{name}/SKILL.md`
    /// container. Returned paths are canonicalized (matches the loader contract).
    #[test]
    fn discover_finds_skill_dirs() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for name in ["alpha", "beta", "gamma"] {
            let skill = root.join(name);
            fs::create_dir_all(&skill).unwrap();
            fs::write(skill.join("SKILL.md"), "# skill").unwrap();
        }
        let mut found = discover_skill_dirs(root, 10);
        found.sort();
        let mut expected: Vec<PathBuf> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|n| std::fs::canonicalize(root.join(n)).unwrap())
            .collect();
        expected.sort();
        assert_eq!(found, expected);
    }

    /// Regression guard for the gstack-style fd exhaustion: deeply-nested
    /// `SKILL.md` files (under a real skill's `node_modules`, etc.) must not
    /// be watched, because we never descend below the immediate child level.
    #[test]
    fn discover_does_not_descend_into_skill_subtrees() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // A real skill that should be discovered.
        let real = root.join("real_skill");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("SKILL.md"), "# skill").unwrap();

        // Decoy SKILL.md files buried deep — these mimic the gstack scenario
        // where a skill ships with `node_modules` that contain unrelated
        // documentation files. With single-level discovery these are never
        // visited.
        for nested in [
            "real_skill/node_modules/foo/inner",
            "real_skill/.git/sub",
            "real_skill/target/debug",
            "deeply/nested/never/reached",
        ] {
            let p = root.join(nested);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("SKILL.md"), "# decoy").unwrap();
        }

        let found = discover_skill_dirs(root, 10);
        let canonical_real = std::fs::canonicalize(&real).unwrap();
        assert_eq!(found, vec![canonical_real]);
    }

    /// A pathological flat root with too many sibling skill dirs trips the
    /// cap; we stop adding watches rather than open thousands of kqueue fds.
    #[test]
    fn discover_respects_cap() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for i in 0..50 {
            let skill = root.join(format!("s{i:03}"));
            fs::create_dir_all(&skill).unwrap();
            fs::write(skill.join("SKILL.md"), "# skill").unwrap();
        }
        let found = discover_skill_dirs(root, 10);
        assert_eq!(found.len(), 10);
    }

    /// Symlinked skills whose canonical path resolves *outside* the search
    /// root must be rejected — same containment rule as the loader.
    #[cfg(unix)]
    #[test]
    fn discover_rejects_skills_escaping_root_via_symlink() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("inside");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();

        // A real skill living inside the root.
        let inside_skill = root.join("legit");
        fs::create_dir_all(&inside_skill).unwrap();
        fs::write(inside_skill.join("SKILL.md"), "# legit").unwrap();

        // A skill outside the root, surfaced via a symlink under the root.
        let outside_skill = outside.join("rogue");
        fs::create_dir_all(&outside_skill).unwrap();
        fs::write(outside_skill.join("SKILL.md"), "# rogue").unwrap();
        std::os::unix::fs::symlink(&outside_skill, root.join("rogue")).unwrap();

        let found = discover_skill_dirs(&root, 10);
        let canonical_legit = std::fs::canonicalize(&inside_skill).unwrap();
        assert_eq!(found, vec![canonical_legit]);
    }

    /// Dedup contract: two symlinks pointing at the same physical skill
    /// produce one watched directory, not two.
    #[cfg(unix)]
    #[test]
    fn discover_dedups_symlinks_to_same_skill() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let physical = root.join("real");
        fs::create_dir_all(&physical).unwrap();
        fs::write(physical.join("SKILL.md"), "# skill").unwrap();

        // Two additional aliases pointing at the same physical dir.
        std::os::unix::fs::symlink(&physical, root.join("alias_a")).unwrap();
        std::os::unix::fs::symlink(&physical, root.join("alias_b")).unwrap();

        let found = discover_skill_dirs(root, 10);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], std::fs::canonicalize(&physical).unwrap());
    }
}
