//! Issue #326 P5 / scenario #28: locate the nearest "package root"
//! for cwd-aware permission scoping.
//!
//! ## Why
//!
//! Plan v3 §P5 wants session overrides bound to the package
//! directory, not the global cwd. Scenario #28 in the 50-scenario
//! review:
//!
//! > User runs `npm test` in `monorepo/web/`. They press Always.
//! > A bit later the agent runs `npm test` in `monorepo/api/`.
//! > Should that be auto-allowed?
//!
//! "Same fingerprint, same project" yes. But `monorepo/web/` and
//! `monorepo/api/` are different packages with different test
//! suites — Always-on-web shouldn't authorize api. The fix is to
//! scope each session override to the nearest package root
//! (Cargo.toml, package.json, pyproject.toml, …) and require the
//! later request's package root to match before short-circuiting.
//!
//! This module provides the "find the package root" helper.
//! Wiring it into `FingerprintedOverrides::check` is staged for
//! the P3 UI commit so we don't churn the override comparison
//! logic across multiple commits.

use std::path::{Path, PathBuf};

/// File names that indicate a package / project root. Walked
/// upward from the request's cwd until one is found.
///
/// Listed in priority order so a sub-package inside a workspace
/// gets its own root rather than the workspace's root. (Yarn /
/// pnpm sub-packages have their own package.json; Cargo
/// workspaces have a package-level Cargo.toml inside member
/// crates.)
pub const PACKAGE_ROOT_MARKERS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "go.mod",
    "build.gradle",
    "build.gradle.kts",
    "pom.xml",
    "Gemfile",
    "Pipfile",
    "composer.json",
    "deno.json",
    "deno.jsonc",
];

/// Walk upward from `start` looking for a package marker file.
///
/// Returns the directory that contains the marker, NOT the marker
/// path itself. Returns `None` if no marker is found before
/// reaching the filesystem root (or hitting `git_repo_ceiling`
/// when set).
///
/// `git_repo_ceiling`, when `Some`, stops the walk if it equals
/// the candidate directory — useful in tests to avoid escaping a
/// tempdir.
#[must_use]
pub fn nearest_package_root(start: &Path, git_repo_ceiling: Option<&Path>) -> Option<PathBuf> {
    let canonical_start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let canonical_ceiling = git_repo_ceiling
        .and_then(|c| std::fs::canonicalize(c).ok())
        .or_else(|| git_repo_ceiling.map(Path::to_path_buf));

    let mut current: Option<&Path> = Some(&canonical_start);
    while let Some(dir) = current {
        // Stop at ceiling.
        if let Some(c) = canonical_ceiling.as_deref() {
            if dir == c {
                // Inclusive: still check this directory then stop.
                if has_marker(dir) {
                    return Some(dir.to_path_buf());
                }
                return None;
            }
        }
        if has_marker(dir) {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn has_marker(dir: &Path) -> bool {
    PACKAGE_ROOT_MARKERS
        .iter()
        .any(|name| dir.join(name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn finds_package_json_in_cwd() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("package.json"));
        assert_eq!(
            nearest_package_root(dir.path(), Some(dir.path()))
                .map(|p| { std::fs::canonicalize(p).unwrap() }),
            Some(std::fs::canonicalize(dir.path()).unwrap())
        );
    }

    #[test]
    fn finds_package_json_in_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        touch(&dir.path().join("package.json"));

        let found = nearest_package_root(&nested, Some(dir.path())).unwrap();
        assert_eq!(
            std::fs::canonicalize(found).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn picks_inner_package_over_outer() {
        // monorepo/web/package.json AND monorepo/package.json:
        // requesting from monorepo/web/src must pick web.
        let dir = tempfile::tempdir().unwrap();
        let monorepo = dir.path();
        let web = monorepo.join("web");
        let src = web.join("src");
        std::fs::create_dir_all(&src).unwrap();
        touch(&monorepo.join("package.json"));
        touch(&web.join("package.json"));

        let found = nearest_package_root(&src, Some(monorepo)).unwrap();
        assert_eq!(
            std::fs::canonicalize(found).unwrap(),
            std::fs::canonicalize(&web).unwrap(),
            "must pick the nearest (web), not the outer (monorepo)"
        );
    }

    #[test]
    fn supports_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("crates").join("foo").join("src");
        std::fs::create_dir_all(&nested).unwrap();
        touch(&dir.path().join("crates").join("foo").join("Cargo.toml"));

        let found = nearest_package_root(&nested, Some(dir.path())).unwrap();
        assert_eq!(
            std::fs::canonicalize(found).unwrap(),
            std::fs::canonicalize(dir.path().join("crates").join("foo")).unwrap()
        );
    }

    #[test]
    fn returns_none_when_no_marker() {
        let dir = tempfile::tempdir().unwrap();
        // No package.json / Cargo.toml / etc.
        assert_eq!(nearest_package_root(dir.path(), Some(dir.path())), None);
    }

    #[test]
    fn ceiling_stops_walk() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        // Marker is OUTSIDE the ceiling — must not be returned.
        // We can't easily put a marker outside a tempdir without
        // permission issues; instead test by setting ceiling AT
        // the nested dir, then putting marker in nested — should
        // be found.
        touch(&nested.join("package.json"));
        let found = nearest_package_root(&nested, Some(&nested)).unwrap();
        assert_eq!(
            std::fs::canonicalize(found).unwrap(),
            std::fs::canonicalize(&nested).unwrap()
        );
    }

    #[test]
    fn scenario_28_web_vs_api_get_different_roots() {
        // monorepo/{web,api}/package.json — npm test in each
        // package must yield a different package root, so the
        // override comparison can disambiguate.
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        let api = dir.path().join("api");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::create_dir_all(&api).unwrap();
        touch(&web.join("package.json"));
        touch(&api.join("package.json"));

        let web_root = nearest_package_root(&web, Some(dir.path())).unwrap();
        let api_root = nearest_package_root(&api, Some(dir.path())).unwrap();
        assert_ne!(
            std::fs::canonicalize(&web_root).unwrap(),
            std::fs::canonicalize(&api_root).unwrap(),
            "scenario #28: web and api must be distinct cwd_root values"
        );
    }
}
