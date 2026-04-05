//! Semantic versioning and dependency resolution for skills.
//!
//! Supports pip-style constraint syntax (`>=1.0`, `<2.0`, `~=1.2.3`, `==1.0.0`, `!=1.5.0`, `*`).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::str::FromStr;

// ── Version ──────────────────────────────────────────────────────────────────

/// A semantic version (major.minor.patch) with optional pre-release tag.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub pre: Option<String>,
}

impl Version {
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: None,
        }
    }

    pub fn with_pre(mut self, pre: impl Into<String>) -> Self {
        self.pre = Some(pre.into());
        self
    }
}

impl Default for Version {
    fn default() -> Self {
        Self::new(0, 1, 0)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(ref pre) = self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then(match (&self.pre, &other.pre) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater, // 1.0.0 > 1.0.0-alpha
                (Some(_), None) => std::cmp::Ordering::Less,    // 1.0.0-alpha < 1.0.0
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (version_part, pre) = if let Some(idx) = s.find('-') {
            (&s[..idx], Some(s[idx + 1..].to_string()))
        } else {
            (s, None)
        };

        let parts: Vec<&str> = version_part.split('.').collect();
        if parts.is_empty() || parts.len() > 3 {
            return Err(format!("invalid version: {s}"));
        }

        let major = parts[0]
            .parse::<u32>()
            .map_err(|_| format!("invalid major version in: {s}"))?;
        let minor = parts
            .get(1)
            .map(|p| p.parse::<u32>())
            .transpose()
            .map_err(|_| format!("invalid minor version in: {s}"))?
            .unwrap_or(0);
        let patch = parts
            .get(2)
            .map(|p| p.parse::<u32>())
            .transpose()
            .map_err(|_| format!("invalid patch version in: {s}"))?
            .unwrap_or(0);

        Ok(Version {
            major,
            minor,
            patch,
            pre,
        })
    }
}

// ── VersionConstraint ────────────────────────────────────────────────────────

/// A single version constraint clause (e.g., `>=1.0` or `<2.0`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConstraintOp {
    /// `==1.0.0` — exact match
    Exact(Version),
    /// `>=1.0` — greater than or equal
    Gte(Version),
    /// `>1.0` — strictly greater than
    Gt(Version),
    /// `<=2.0` — less than or equal
    Lte(Version),
    /// `<2.0` — strictly less than
    Lt(Version),
    /// `!=1.5.0` — not equal
    Ne(Version),
    /// `~=1.2.3` — compatible release (>=1.2.3, <1.3.0)
    /// `~=1.2` — compatible release (>=1.2.0, <2.0.0)
    Compatible(Version),
    /// `*` — any version
    Any,
}

impl ConstraintOp {
    /// Check if a version satisfies this constraint.
    pub fn matches(&self, v: &Version) -> bool {
        match self {
            ConstraintOp::Exact(c) => v == c,
            ConstraintOp::Gte(c) => v >= c,
            ConstraintOp::Gt(c) => v > c,
            ConstraintOp::Lte(c) => v <= c,
            ConstraintOp::Lt(c) => v < c,
            ConstraintOp::Ne(c) => v != c,
            ConstraintOp::Compatible(c) => {
                if c.patch > 0 || c.pre.is_some() {
                    // ~=1.2.3 means >=1.2.3, <1.3.0
                    let upper = Version::new(c.major, c.minor + 1, 0);
                    v >= c && v < &upper
                } else {
                    // ~=1.2 means >=1.2.0, <2.0.0
                    let upper = Version::new(c.major + 1, 0, 0);
                    v >= c && v < &upper
                }
            }
            ConstraintOp::Any => true,
        }
    }
}

impl fmt::Display for ConstraintOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConstraintOp::Exact(v) => write!(f, "=={v}"),
            ConstraintOp::Gte(v) => write!(f, ">={v}"),
            ConstraintOp::Gt(v) => write!(f, ">{v}"),
            ConstraintOp::Lte(v) => write!(f, "<={v}"),
            ConstraintOp::Lt(v) => write!(f, "<{v}"),
            ConstraintOp::Ne(v) => write!(f, "!={v}"),
            ConstraintOp::Compatible(v) => write!(f, "~={v}"),
            ConstraintOp::Any => write!(f, "*"),
        }
    }
}

/// A version constraint composed of one or more clauses (AND-combined).
/// Parsed from strings like `">=1.0,<2.0"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionConstraint {
    pub clauses: Vec<ConstraintOp>,
}

impl VersionConstraint {
    pub fn any() -> Self {
        Self {
            clauses: vec![ConstraintOp::Any],
        }
    }

    pub fn exact(v: Version) -> Self {
        Self {
            clauses: vec![ConstraintOp::Exact(v)],
        }
    }

    /// Check if a version satisfies all clauses.
    pub fn matches(&self, v: &Version) -> bool {
        self.clauses.iter().all(|c| c.matches(v))
    }
}

impl Default for VersionConstraint {
    fn default() -> Self {
        Self::any()
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.clauses.iter().map(|c| c.to_string()).collect();
        write!(f, "{}", parts.join(","))
    }
}

impl FromStr for VersionConstraint {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s == "*" || s.is_empty() {
            return Ok(VersionConstraint::any());
        }

        let clauses: Result<Vec<ConstraintOp>, String> =
            s.split(',').map(|part| parse_constraint_op(part.trim())).collect();

        Ok(VersionConstraint {
            clauses: clauses?,
        })
    }
}

fn parse_constraint_op(s: &str) -> Result<ConstraintOp, String> {
    let s = s.trim();
    if s == "*" {
        return Ok(ConstraintOp::Any);
    }

    if let Some(rest) = s.strip_prefix("~=") {
        let v: Version = rest.trim().parse()?;
        return Ok(ConstraintOp::Compatible(v));
    }
    if let Some(rest) = s.strip_prefix(">=") {
        let v: Version = rest.trim().parse()?;
        return Ok(ConstraintOp::Gte(v));
    }
    if let Some(rest) = s.strip_prefix("<=") {
        let v: Version = rest.trim().parse()?;
        return Ok(ConstraintOp::Lte(v));
    }
    if let Some(rest) = s.strip_prefix("!=") {
        let v: Version = rest.trim().parse()?;
        return Ok(ConstraintOp::Ne(v));
    }
    if let Some(rest) = s.strip_prefix("==") {
        let v: Version = rest.trim().parse()?;
        return Ok(ConstraintOp::Exact(v));
    }
    if let Some(rest) = s.strip_prefix('>') {
        let v: Version = rest.trim().parse()?;
        return Ok(ConstraintOp::Gt(v));
    }
    if let Some(rest) = s.strip_prefix('<') {
        let v: Version = rest.trim().parse()?;
        return Ok(ConstraintOp::Lt(v));
    }

    // Bare version = exact
    let v: Version = s.parse()?;
    Ok(ConstraintOp::Exact(v))
}

// ── Dependency ───────────────────────────────────────────────────────────────

/// The type of a dependency.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyType {
    Skill,
    Tool,
}

impl Default for DependencyType {
    fn default() -> Self {
        DependencyType::Skill
    }
}

/// A skill dependency with name, version constraint, and type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    #[serde(default)]
    pub version: VersionConstraint,
    #[serde(default, rename = "type")]
    pub dep_type: DependencyType,
}

// ── DependencyResolver ───────────────────────────────────────────────────────

/// Result of dependency resolution.
#[derive(Clone, Debug)]
pub struct ResolveResult {
    /// Successfully resolved: topologically sorted dependency names.
    pub ordered: Vec<String>,
    /// Version conflicts (dependency name -> conflicting constraints).
    pub conflicts: Vec<VersionConflict>,
    /// Missing dependencies.
    pub missing: Vec<String>,
    /// Detected cycles.
    pub cycles: Vec<Vec<String>>,
}

impl ResolveResult {
    pub fn is_ok(&self) -> bool {
        self.conflicts.is_empty() && self.missing.is_empty() && self.cycles.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct VersionConflict {
    pub dependency: String,
    pub available: Option<Version>,
    pub required_by: Vec<(String, VersionConstraint)>,
}

/// Resolves a dependency graph, detecting cycles, missing deps, and version conflicts.
pub struct DependencyResolver {
    /// Available skills: name -> version.
    available: HashMap<String, Version>,
    /// Dependency edges: skill name -> its declared dependencies.
    dep_graph: HashMap<String, Vec<Dependency>>,
}

impl DependencyResolver {
    pub fn new() -> Self {
        Self {
            available: HashMap::new(),
            dep_graph: HashMap::new(),
        }
    }

    /// Register a skill as available.
    pub fn add_available(&mut self, name: impl Into<String>, version: Version) {
        self.available.insert(name.into(), version);
    }

    /// Register a skill's dependencies.
    pub fn add_dependencies(&mut self, name: impl Into<String>, deps: Vec<Dependency>) {
        self.dep_graph.insert(name.into(), deps);
    }

    /// Resolve the full dependency graph starting from the given root skills.
    pub fn resolve(&self, roots: &[String]) -> ResolveResult {
        let mut missing = Vec::new();
        let mut conflicts = Vec::new();
        let cycles = self.detect_cycles();

        // Check all dependencies for availability and version compatibility
        let mut constraint_map: HashMap<String, Vec<(String, VersionConstraint)>> = HashMap::new();

        for (skill, deps) in &self.dep_graph {
            if !roots.is_empty() && !roots.contains(skill) {
                continue;
            }
            for dep in deps {
                if dep.dep_type == DependencyType::Tool {
                    continue; // Tools are not resolved here
                }
                constraint_map
                    .entry(dep.name.clone())
                    .or_default()
                    .push((skill.clone(), dep.version.clone()));
            }
        }

        for (dep_name, requestors) in &constraint_map {
            match self.available.get(dep_name) {
                None => {
                    if !missing.contains(dep_name) {
                        missing.push(dep_name.clone());
                    }
                }
                Some(available_version) => {
                    let unsatisfied: Vec<_> = requestors
                        .iter()
                        .filter(|(_, constraint)| !constraint.matches(available_version))
                        .cloned()
                        .collect();

                    if !unsatisfied.is_empty() {
                        conflicts.push(VersionConflict {
                            dependency: dep_name.clone(),
                            available: Some(available_version.clone()),
                            required_by: unsatisfied,
                        });
                    }
                }
            }
        }

        // Topological sort (Kahn's algorithm)
        let ordered = if cycles.is_empty() {
            self.topological_sort(roots)
        } else {
            Vec::new()
        };

        ResolveResult {
            ordered,
            conflicts,
            missing,
            cycles,
        }
    }

    fn detect_cycles(&self) -> Vec<Vec<String>> {
        let mut cycles = Vec::new();
        let mut visited = HashSet::new();
        let mut stack = HashSet::new();
        let mut path = Vec::new();

        for name in self.dep_graph.keys() {
            if !visited.contains(name) {
                self.dfs_cycle(name, &mut visited, &mut stack, &mut path, &mut cycles);
            }
        }

        cycles
    }

    fn dfs_cycle(
        &self,
        node: &str,
        visited: &mut HashSet<String>,
        stack: &mut HashSet<String>,
        path: &mut Vec<String>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        visited.insert(node.to_string());
        stack.insert(node.to_string());
        path.push(node.to_string());

        if let Some(deps) = self.dep_graph.get(node) {
            for dep in deps {
                if dep.dep_type == DependencyType::Tool {
                    continue;
                }
                if !visited.contains(&dep.name) {
                    self.dfs_cycle(&dep.name, visited, stack, path, cycles);
                } else if stack.contains(&dep.name) {
                    // Found a cycle — extract it
                    if let Some(start) = path.iter().position(|n| n == &dep.name) {
                        let mut cycle: Vec<String> = path[start..].to_vec();
                        cycle.push(dep.name.clone());
                        cycles.push(cycle);
                    }
                }
            }
        }

        stack.remove(node);
        path.pop();
    }

    fn topological_sort(&self, roots: &[String]) -> Vec<String> {
        let relevant: HashSet<&String> = if roots.is_empty() {
            self.dep_graph.keys().collect()
        } else {
            roots.iter().collect()
        };

        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();

        for name in &relevant {
            in_degree.entry(name.to_string()).or_insert(0);
            if let Some(deps) = self.dep_graph.get(name.as_str()) {
                for dep in deps {
                    if dep.dep_type == DependencyType::Tool {
                        continue;
                    }
                    adj.entry(dep.name.clone())
                        .or_default()
                        .push(name.to_string());
                    *in_degree.entry(name.to_string()).or_insert(0) += 1;
                    in_degree.entry(dep.name.clone()).or_insert(0);
                }
            }
        }

        let mut queue: VecDeque<String> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(name, _)| name.clone())
            .collect();

        let mut result = Vec::new();
        while let Some(node) = queue.pop_front() {
            result.push(node.clone());
            if let Some(dependents) = adj.get(&node) {
                for dep in dependents {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }

        result
    }
}

impl Default for DependencyResolver {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_full() {
        let v: Version = "1.2.3".parse().unwrap();
        assert_eq!(v, Version::new(1, 2, 3));
    }

    #[test]
    fn version_parse_major_minor() {
        let v: Version = "2.1".parse().unwrap();
        assert_eq!(v, Version::new(2, 1, 0));
    }

    #[test]
    fn version_parse_major_only() {
        let v: Version = "3".parse().unwrap();
        assert_eq!(v, Version::new(3, 0, 0));
    }

    #[test]
    fn version_parse_pre() {
        let v: Version = "1.0.0-beta".parse().unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.pre, Some("beta".to_string()));
    }

    #[test]
    fn version_ordering() {
        let v1: Version = "1.0.0".parse().unwrap();
        let v2: Version = "1.0.1".parse().unwrap();
        let v3: Version = "1.1.0".parse().unwrap();
        let v4: Version = "2.0.0".parse().unwrap();
        assert!(v1 < v2);
        assert!(v2 < v3);
        assert!(v3 < v4);
    }

    #[test]
    fn version_ordering_prerelease() {
        let alpha: Version = "1.0.0-alpha".parse().unwrap();
        let beta: Version = "1.0.0-beta".parse().unwrap();
        let release: Version = "1.0.0".parse().unwrap();

        assert!(alpha < beta, "alpha < beta");
        assert!(beta < release, "beta < release");
        assert!(alpha < release, "alpha < release");
        assert_eq!(alpha.cmp(&alpha), std::cmp::Ordering::Equal);

        // Pre-release should NOT match >=1.0.0
        let gte = ConstraintOp::Gte(Version::new(1, 0, 0));
        assert!(!gte.matches(&alpha), "1.0.0-alpha should not match >=1.0.0");
        assert!(gte.matches(&release), "1.0.0 should match >=1.0.0");
    }

    #[test]
    fn constraint_gte() {
        let c = ConstraintOp::Gte(Version::new(1, 0, 0));
        assert!(c.matches(&Version::new(1, 0, 0)));
        assert!(c.matches(&Version::new(1, 5, 0)));
        assert!(c.matches(&Version::new(2, 0, 0)));
        assert!(!c.matches(&Version::new(0, 9, 9)));
    }

    #[test]
    fn constraint_lt() {
        let c = ConstraintOp::Lt(Version::new(2, 0, 0));
        assert!(c.matches(&Version::new(1, 9, 9)));
        assert!(!c.matches(&Version::new(2, 0, 0)));
        assert!(!c.matches(&Version::new(2, 0, 1)));
    }

    #[test]
    fn constraint_compatible_three_part() {
        // ~=1.2.3 means >=1.2.3, <1.3.0
        let c = ConstraintOp::Compatible(Version::new(1, 2, 3));
        assert!(c.matches(&Version::new(1, 2, 3)));
        assert!(c.matches(&Version::new(1, 2, 9)));
        assert!(!c.matches(&Version::new(1, 3, 0)));
        assert!(!c.matches(&Version::new(1, 2, 2)));
    }

    #[test]
    fn constraint_compatible_two_part() {
        // ~=1.2 means >=1.2.0, <2.0.0
        let c = ConstraintOp::Compatible(Version::new(1, 2, 0));
        assert!(c.matches(&Version::new(1, 2, 0)));
        assert!(c.matches(&Version::new(1, 9, 9)));
        assert!(!c.matches(&Version::new(2, 0, 0)));
        assert!(!c.matches(&Version::new(1, 1, 9)));
    }

    #[test]
    fn constraint_ne() {
        let c = ConstraintOp::Ne(Version::new(1, 5, 0));
        assert!(c.matches(&Version::new(1, 4, 9)));
        assert!(c.matches(&Version::new(1, 5, 1)));
        assert!(!c.matches(&Version::new(1, 5, 0)));
    }

    #[test]
    fn version_constraint_parse_range() {
        let vc: VersionConstraint = ">=1.0,<2.0".parse().unwrap();
        assert!(vc.matches(&Version::new(1, 0, 0)));
        assert!(vc.matches(&Version::new(1, 9, 9)));
        assert!(!vc.matches(&Version::new(2, 0, 0)));
        assert!(!vc.matches(&Version::new(0, 9, 9)));
    }

    #[test]
    fn version_constraint_parse_any() {
        let vc: VersionConstraint = "*".parse().unwrap();
        assert!(vc.matches(&Version::new(0, 0, 1)));
        assert!(vc.matches(&Version::new(99, 99, 99)));
    }

    #[test]
    fn version_constraint_parse_exact() {
        let vc: VersionConstraint = "==1.0.0".parse().unwrap();
        assert!(vc.matches(&Version::new(1, 0, 0)));
        assert!(!vc.matches(&Version::new(1, 0, 1)));
    }

    #[test]
    fn version_constraint_display_roundtrip() {
        let vc: VersionConstraint = ">=1.0.0,<2.0.0".parse().unwrap();
        let s = vc.to_string();
        let vc2: VersionConstraint = s.parse().unwrap();
        assert_eq!(vc, vc2);
    }

    #[test]
    fn resolver_detects_missing_deps() {
        let mut resolver = DependencyResolver::new();
        resolver.add_available("my_skill", Version::new(1, 0, 0));
        resolver.add_dependencies(
            "my_skill",
            vec![Dependency {
                name: "nonexistent".into(),
                version: VersionConstraint::any(),
                dep_type: DependencyType::Skill,
            }],
        );

        let result = resolver.resolve(&["my_skill".into()]);
        assert!(!result.is_ok());
        assert_eq!(result.missing, vec!["nonexistent"]);
    }

    #[test]
    fn resolver_detects_version_conflict() {
        let mut resolver = DependencyResolver::new();
        resolver.add_available("base", Version::new(1, 0, 0));
        resolver.add_available("a", Version::new(1, 0, 0));
        resolver.add_dependencies(
            "a",
            vec![Dependency {
                name: "base".into(),
                version: ">=2.0".parse().unwrap(),
                dep_type: DependencyType::Skill,
            }],
        );

        let result = resolver.resolve(&["a".into()]);
        assert!(!result.is_ok());
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].dependency, "base");
    }

    #[test]
    fn resolver_detects_cycles() {
        let mut resolver = DependencyResolver::new();
        resolver.add_available("a", Version::new(1, 0, 0));
        resolver.add_available("b", Version::new(1, 0, 0));
        resolver.add_available("c", Version::new(1, 0, 0));
        resolver.add_dependencies(
            "a",
            vec![Dependency {
                name: "b".into(),
                version: VersionConstraint::any(),
                dep_type: DependencyType::Skill,
            }],
        );
        resolver.add_dependencies(
            "b",
            vec![Dependency {
                name: "c".into(),
                version: VersionConstraint::any(),
                dep_type: DependencyType::Skill,
            }],
        );
        resolver.add_dependencies(
            "c",
            vec![Dependency {
                name: "a".into(),
                version: VersionConstraint::any(),
                dep_type: DependencyType::Skill,
            }],
        );

        let result = resolver.resolve(&[]);
        assert!(!result.cycles.is_empty());
    }

    #[test]
    fn resolver_topological_sort() {
        let mut resolver = DependencyResolver::new();
        resolver.add_available("base", Version::new(1, 0, 0));
        resolver.add_available("mid", Version::new(1, 0, 0));
        resolver.add_available("top", Version::new(1, 0, 0));
        resolver.add_dependencies("top", vec![
            Dependency {
                name: "mid".into(),
                version: VersionConstraint::any(),
                dep_type: DependencyType::Skill,
            },
        ]);
        resolver.add_dependencies("mid", vec![
            Dependency {
                name: "base".into(),
                version: VersionConstraint::any(),
                dep_type: DependencyType::Skill,
            },
        ]);

        let result = resolver.resolve(&["top".into()]);
        assert!(result.is_ok());
        // base should come before mid, mid before top
        let base_idx = result.ordered.iter().position(|n| n == "base");
        let mid_idx = result.ordered.iter().position(|n| n == "mid");
        let top_idx = result.ordered.iter().position(|n| n == "top");
        assert!(base_idx < mid_idx);
        assert!(mid_idx < top_idx);
    }
}
