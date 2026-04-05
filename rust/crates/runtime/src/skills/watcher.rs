//! File-system watcher for skill hot-reload.
//!
//! Watches skill search paths (`.astra/skills/`, `skills/`, `~/.astra/skills/`)
//! for changes and triggers a registry re-discover when SKILL.md or manifest.yaml
//! files are created, modified, or deleted.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use super::registry::UnifiedSkillRegistry;

/// Default debounce interval for skill file changes.
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(500);

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

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(event) = res {
            if is_skill_relevant_event(&event) {
                let _ = tx.send(());
            }
        }
    })
    .ok()?;

    for path in &existing_paths {
        if watcher.watch(path, RecursiveMode::Recursive).is_err() {
            eprintln!("  ⚠ Failed to watch: {}", path.display());
        }
    }

    let task = tokio::spawn(debounced_refresh_loop(registry, rx));

    Some(SkillWatcherHandle {
        _watcher: watcher,
        _task: task,
    })
}

/// Returns true if the filesystem event is relevant to skill files.
fn is_skill_relevant_event(event: &notify::Event) -> bool {
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
        _ => return false,
    }

    event.paths.iter().any(|p| {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        name == "SKILL.md"
            || name == "manifest.yaml"
            || name == "manifest.yml"
            || name == "skill.json"
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
                eprintln!(
                    "  🔄 Skills reloaded ({} discovered)",
                    names.len()
                );
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
        // Drop handle to stop
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
}
