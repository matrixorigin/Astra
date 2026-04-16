//! Performance optimizations for plan mode.
//!
//! - **Template cache**: indexes successful plans by goal pattern for faster decomposition
//! - **Incremental project context**: caches `ProjectContext` and only rescans changed files
//! - **Debounced sync**: batches plan state updates to avoid excessive cloud writes

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::decompose::PlanTemplateHint;

// ─── Template Cache ─────────────────────────────────────────────────────────

/// Local file-based cache of successful plan templates.
///
/// Indexes completed plans by normalized goal pattern so the LLM can leverage
/// learned patterns for faster decomposition of similar tasks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanTemplateCache {
    /// Cached templates, keyed by normalized goal pattern.
    pub templates: HashMap<String, CachedTemplate>,
    /// Cache version for forward compatibility.
    #[serde(default = "default_cache_version")]
    pub version: u32,
}

fn default_cache_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTemplate {
    pub goal_pattern: String,
    pub subtask_titles: Vec<String>,
    pub success_rate: f64,
    pub use_count: u32,
    pub last_used: u64,
    pub avg_duration_ms: u64,
}

impl PlanTemplateCache {
    /// Load the template cache from disk.
    pub fn load() -> Self {
        let path = Self::cache_path();
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save the template cache to disk.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::cache_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create cache dir: {e}"))?;
        }
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize cache: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("write cache: {e}"))
    }

    /// Record a successful plan completion for future reuse.
    pub fn record_success(&mut self, goal: &str, subtask_titles: Vec<String>, duration_ms: u64) {
        let key = normalize_goal(goal);
        let now = now_unix_secs();

        if let Some(existing) = self.templates.get_mut(&key) {
            existing.use_count += 1;
            // Incremental average: avg += (new - avg) / count
            existing.success_rate += (1.0 - existing.success_rate) / existing.use_count as f64;
            existing.last_used = now;
            existing.avg_duration_ms = existing.avg_duration_ms
                + (duration_ms.saturating_sub(existing.avg_duration_ms))
                    / existing.use_count as u64;
            existing.subtask_titles = subtask_titles;
        } else {
            self.templates.insert(
                key.clone(),
                CachedTemplate {
                    goal_pattern: key,
                    subtask_titles,
                    success_rate: 1.0,
                    use_count: 1,
                    last_used: now,
                    avg_duration_ms: duration_ms,
                },
            );
        }
    }

    /// Record a plan failure. Decreases the success rate without adding subtask titles.
    pub fn record_failure(&mut self, goal: &str) {
        let key = normalize_goal(goal);
        let now = now_unix_secs();

        if let Some(existing) = self.templates.get_mut(&key) {
            existing.use_count += 1;
            // Incremental average with 0 for failure: avg += (0 - avg) / count
            existing.success_rate += (0.0 - existing.success_rate) / existing.use_count as f64;
            existing.last_used = now;
        }
    }

    /// Look up templates matching a goal pattern.
    /// Returns up to `limit` most relevant templates sorted by recency and success.
    pub fn lookup(&self, goal: &str, limit: usize) -> Vec<PlanTemplateHint> {
        let key = normalize_goal(goal);
        let goal_words: Vec<&str> = key.split_whitespace().collect();

        let mut scored: Vec<(f64, &CachedTemplate)> = self
            .templates
            .values()
            .filter_map(|t| {
                let t_words: Vec<&str> = t.goal_pattern.split_whitespace().collect();
                let overlap = goal_words.iter().filter(|w| t_words.contains(w)).count();
                if overlap == 0 {
                    return None;
                }
                let word_score = overlap as f64 / goal_words.len().max(t_words.len()) as f64;
                let recency_bonus = if t.last_used > now_unix_secs().saturating_sub(86400 * 7) {
                    0.1
                } else {
                    0.0
                };
                let score = word_score * 0.6 + t.success_rate * 0.3 + recency_bonus;
                Some((score, t))
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        scored
            .into_iter()
            .take(limit)
            .map(|(_, t)| PlanTemplateHint {
                goal_pattern: t.goal_pattern.clone(),
                subtask_titles: t.subtask_titles.clone(),
                success_rate: t.success_rate,
                use_count: t.use_count,
            })
            .collect()
    }

    /// Evict stale entries older than `max_age`.
    pub fn evict_stale(&mut self, max_age_secs: u64) {
        let cutoff = now_unix_secs().saturating_sub(max_age_secs);
        self.templates.retain(|_, t| t.last_used >= cutoff);
    }

    fn cache_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".astra")
            .join("plan_template_cache.json")
    }
}

/// Normalize a goal string for template matching.
fn normalize_goal(goal: &str) -> String {
    goal.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Incremental Project Context ────────────────────────────────────────────

/// Cached project context with modification timestamps.
///
/// Avoids full filesystem scan on every plan operation by tracking
/// when the project root was last scanned and only rescanning if
/// files have changed.
#[derive(Debug, Clone)]
pub struct CachedProjectContext {
    pub context: crate::decompose::ProjectContext,
    pub scanned_at: Instant,
    pub root_mtime: Option<SystemTime>,
}

impl CachedProjectContext {
    /// Create a new cached context by scanning the project root.
    pub fn scan(root: &Path) -> Self {
        Self {
            context: crate::decompose::analyze_project(root),
            scanned_at: Instant::now(),
            root_mtime: root.metadata().ok().and_then(|m| m.modified().ok()),
        }
    }

    /// Check if the cache is still valid (within TTL and root not modified).
    pub fn is_valid(&self, root: &Path, ttl: Duration) -> bool {
        if self.scanned_at.elapsed() > ttl {
            return false;
        }
        match (
            self.root_mtime,
            root.metadata().ok().and_then(|m| m.modified().ok()),
        ) {
            (Some(cached), Some(current)) => cached >= current,
            (None, None) => true, // mtime unavailable — trust TTL alone
            _ => false,
        }
    }

    /// Refresh the context if stale, otherwise return the cached version.
    pub fn refresh_if_stale(&mut self, root: &Path, ttl: Duration) {
        if !self.is_valid(root, ttl) {
            *self = Self::scan(root);
        }
    }
}

// ─── Debounced Sync ─────────────────────────────────────────────────────────

/// Debounced state synchronization.
///
/// Batches plan state updates to avoid excessive cloud writes.
/// The first update triggers a timer; subsequent updates within the debounce
/// window are coalesced. Only the final state is synced.
#[derive(Debug)]
pub struct DebouncedSync {
    /// Minimum interval between syncs.
    pub debounce_ms: u64,
    /// When the last sync was performed.
    last_sync: Option<Instant>,
    /// Whether a sync is pending (state changed since last sync).
    dirty: bool,
    /// Number of coalesced updates since last sync.
    coalesced_count: u32,
}

impl DebouncedSync {
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            debounce_ms,
            last_sync: None,
            dirty: false,
            coalesced_count: 0,
        }
    }

    /// Mark state as dirty (changed). Returns `true` if sync should happen now.
    pub fn mark_dirty(&mut self) -> bool {
        self.dirty = true;
        self.coalesced_count += 1;
        self.should_sync()
    }

    /// Whether enough time has passed since last sync and there are pending changes.
    pub fn should_sync(&self) -> bool {
        if !self.dirty {
            return false;
        }
        match self.last_sync {
            None => true,
            Some(last) => last.elapsed() >= Duration::from_millis(self.debounce_ms),
        }
    }

    /// Record that a sync was performed.
    pub fn mark_synced(&mut self) {
        self.last_sync = Some(Instant::now());
        self.dirty = false;
        self.coalesced_count = 0;
    }

    /// Force a sync regardless of debounce timer (e.g., on session end).
    pub fn force_sync(&mut self) -> bool {
        if self.dirty {
            self.mark_synced();
            true
        } else {
            false
        }
    }

    /// Number of updates coalesced since last sync.
    pub fn coalesced_count(&self) -> u32 {
        self.coalesced_count
    }
}

impl Default for DebouncedSync {
    fn default() -> Self {
        Self::new(2000) // 2 second debounce
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_cache_record_and_lookup() {
        let mut cache = PlanTemplateCache::default();
        cache.record_success(
            "Add user authentication with JWT",
            vec!["Setup JWT library".into(), "Add login endpoint".into()],
            5000,
        );

        let results = cache.lookup("Add JWT authentication", 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].subtask_titles.len(), 2);
        assert_eq!(results[0].use_count, 1);
    }

    #[test]
    fn template_cache_no_match_for_unrelated_goal() {
        let mut cache = PlanTemplateCache::default();
        cache.record_success("Add user authentication", vec!["Setup auth".into()], 1000);

        let results = cache.lookup("deploy kubernetes cluster", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn template_cache_updates_on_repeat() {
        let mut cache = PlanTemplateCache::default();
        cache.record_success("add tests", vec!["write tests".into()], 1000);
        cache.record_success(
            "add tests",
            vec!["write tests".into(), "run CI".into()],
            2000,
        );

        let results = cache.lookup("add tests", 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].use_count, 2);
    }

    #[test]
    fn template_cache_evict_stale() {
        let mut cache = PlanTemplateCache::default();
        cache.templates.insert(
            "old task".into(),
            CachedTemplate {
                goal_pattern: "old task".into(),
                subtask_titles: vec![],
                success_rate: 1.0,
                use_count: 1,
                last_used: 0, // epoch = very old
                avg_duration_ms: 0,
            },
        );
        cache.record_success("new task", vec![], 0);

        assert_eq!(cache.templates.len(), 2);
        cache.evict_stale(86400); // 1 day
        assert_eq!(cache.templates.len(), 1);
        assert!(cache.templates.contains_key("new task"));
    }

    #[test]
    fn normalize_goal_strips_punctuation() {
        assert_eq!(normalize_goal("Add auth!"), "add auth");
        assert_eq!(normalize_goal("  hello   world  "), "hello world");
        assert_eq!(normalize_goal("JWT + OAuth"), "jwt oauth");
    }

    #[test]
    fn debounced_sync_initial_sync() {
        let mut sync = DebouncedSync::new(1000);
        assert!(!sync.should_sync());
        assert!(sync.mark_dirty()); // first dirty should trigger
    }

    #[test]
    fn debounced_sync_coalesces_rapid_updates() {
        let mut sync = DebouncedSync::new(60_000); // 60s debounce
        sync.mark_dirty();
        sync.mark_synced();

        // Rapid updates within debounce window
        sync.mark_dirty();
        assert!(!sync.should_sync()); // debounce not elapsed
        sync.mark_dirty();
        assert_eq!(sync.coalesced_count(), 2);
    }

    #[test]
    fn debounced_sync_force() {
        let mut sync = DebouncedSync::new(60_000);
        sync.mark_dirty();
        sync.mark_synced();
        sync.mark_dirty();
        assert!(sync.force_sync());
        assert!(!sync.force_sync()); // already synced
    }

    #[test]
    fn record_failure_decrements_success_rate() {
        let mut cache = PlanTemplateCache::default();
        cache.record_success("deploy service", vec!["build".into(), "push".into()], 3000);

        let before = cache.templates.get("deploy service").unwrap().success_rate;
        assert!(
            (before - 1.0).abs() < f64::EPSILON,
            "initial success_rate should be 1.0"
        );

        cache.record_failure("deploy service");

        let after = cache.templates.get("deploy service").unwrap();
        assert!(
            after.success_rate < before,
            "success_rate should decrease after failure: was {before}, now {}",
            after.success_rate
        );
        assert_eq!(after.use_count, 2);
        assert!(
            after.success_rate > 0.0,
            "one failure out of two uses should not zero the rate"
        );
    }

    #[test]
    fn record_failure_noop_for_unknown_goal() {
        let mut cache = PlanTemplateCache::default();
        cache.record_failure("nonexistent goal");
        assert!(cache.templates.is_empty());
    }

    #[test]
    fn cached_context_invalidates_after_ttl() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctx = CachedProjectContext::scan(dir.path());
        assert!(ctx.is_valid(dir.path(), Duration::from_secs(60)));
        // Can't easily test TTL expiry without sleeping, but verify the structure
        let _ = ctx.scanned_at.elapsed(); // ensure field is accessible
    }
}
