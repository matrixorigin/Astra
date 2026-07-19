//! Semantic versions and dependency declarations for skills.
//!
//! Supports pip-style constraint syntax (`>=1.0`, `<2.0`, `~=1.2.3`, `==1.0.0`, `!=1.5.0`, `*`).

use serde::{Deserialize, Serialize};
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

    /// True if this constraint accepts any version (`*`).
    pub fn is_any(&self) -> bool {
        self.clauses.len() == 1 && matches!(self.clauses[0], ConstraintOp::Any)
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

        let clauses: Result<Vec<ConstraintOp>, String> = s
            .split(',')
            .map(|part| parse_constraint_op(part.trim()))
            .collect();

        Ok(VersionConstraint { clauses: clauses? })
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyType {
    #[default]
    Skill,
    Tool,
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
    fn is_any_returns_true_for_default_constraint() {
        let c = VersionConstraint::default();
        assert!(c.is_any());
        let c2 = VersionConstraint::any();
        assert!(c2.is_any());
    }

    #[test]
    fn is_any_returns_false_for_specific_constraint() {
        let c: VersionConstraint = ">=1.0".parse().unwrap();
        assert!(!c.is_any());
        let c2 = VersionConstraint::exact(Version::new(1, 0, 0));
        assert!(!c2.is_any());
    }
}
