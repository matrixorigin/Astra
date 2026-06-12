//! Cached git-branch detection for the TUI footer / status line / context dump.
//!
//! `gix::discover` walks parent directories and opens repository config; calling
//! it on every render (the TUI footer redraws on every frame) is wasteful and
//! shows up under perf profiling on large monorepos.
//!
//! This module memoises the answer per-cwd with a short TTL. The branch can
//! still change (`git checkout`) between calls — the TTL bounds staleness so
//! the footer reflects reality within ~2 s of a checkout, which is well below
//! human perception of "wrong branch".
//!
//! The cache is keyed by `(cwd, ttl-bucket)` so that switching directories
//! in another shell doesn't cause cross-contamination.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default cache lifetime. Chosen so that:
/// - the TUI footer re-renders many times per second without re-running gix;
/// - a `git checkout` reflects in the footer within ~2 s.
const DEFAULT_TTL: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct CacheEntry {
    cwd: PathBuf,
    branch: Option<String>,
    fetched_at: Instant,
}

static CACHE: Mutex<Option<CacheEntry>> = Mutex::new(None);

/// Cached branch lookup. Returns the short branch name (`main`,
/// `feature/x`), a parenthesised short SHA for detached HEAD
/// (`(abc1234)`), or `None` when the cwd isn't a git repo / on
/// any I/O error.
///
/// Re-runs the underlying `gix` discovery when the cache is empty,
/// older than `DEFAULT_TTL`, or the process cwd changed.
pub fn detect_git_branch_cached() -> Option<String> {
    detect_with_ttl(DEFAULT_TTL)
}

fn detect_with_ttl(ttl: Duration) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let now = Instant::now();
    {
        let guard = CACHE.lock().ok()?;
        if let Some(entry) = guard.as_ref() {
            if entry.cwd == cwd && now.duration_since(entry.fetched_at) < ttl {
                return entry.branch.clone();
            }
        }
    }
    let branch = lookup_branch(&cwd);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(CacheEntry {
            cwd,
            branch: branch.clone(),
            fetched_at: now,
        });
    }
    branch
}

/// The actual one-shot `gix` lookup. Mirrors the previous inline copies:
/// branch name on attached HEAD, parenthesised short SHA on detached HEAD,
/// `None` for non-git / errors.
fn lookup_branch(cwd: &std::path::Path) -> Option<String> {
    let repo = gix::discover(cwd).ok()?;
    let head = repo.head().ok()?;
    if let Some(name) = head.referent_name() {
        return Some(name.shorten().to_string());
    }
    let id = head.id()?;
    Some(format!("({})", id.to_hex_with_len(7)))
}

/// Test-only: clear the cache so unit tests are deterministic.
#[cfg(test)]
fn clear_cache() {
    if let Ok(mut g) = CACHE.lock() {
        *g = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{clear_cache, detect_git_branch_cached, detect_with_ttl, lookup_branch};
    use std::time::{Duration, Instant};

    #[test]
    fn cache_hit_is_fast_and_stale_refreshes() {
        clear_cache();

        // TTL=2s: 1000 consecutive cached calls must be fast
        let first = detect_git_branch_cached();
        let start = Instant::now();
        for _ in 0..1000 {
            let _ = detect_git_branch_cached();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "1000 cached lookups took {elapsed:?}; cache likely bypassed"
        );
        let again = detect_git_branch_cached();
        assert_eq!(first, again, "cached branch must be stable within TTL");

        // TTL=0: every call refreshes — must match direct lookup
        clear_cache();
        let cwd = std::env::current_dir().unwrap();
        let direct = lookup_branch(&cwd);
        let cached = detect_with_ttl(Duration::from_secs(0));
        assert_eq!(direct, cached, "zero-TTL must match direct lookup");
    }
}
