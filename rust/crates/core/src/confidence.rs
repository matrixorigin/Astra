//! Confidence interval — typed replacement for bare `f64` confidence values.
//!
//! A `ConfidenceInterval` carries a point estimate plus lower/upper bounds,
//! preventing silent loss of uncertainty information.  The runtime enforces
//! the invariant `0.0 ≤ lower ≤ point ≤ upper ≤ 1.0`.
//!
//! ```
//! use astra_core::confidence::ConfidenceInterval;
//!
//! let ci = ConfidenceInterval::new(0.8, 0.65, 0.9);
//! assert!(ci.is_high());
//! assert!(!ci.is_uncertain());
//! ```

use serde::{Deserialize, Serialize};

/// A confidence value with explicit uncertainty bounds.
///
/// Invariant: `0.0 ≤ lower ≤ point ≤ upper ≤ 1.0`.
/// Constructors clamp inputs to satisfy this.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ConfidenceInterval {
    /// Point estimate (most likely value).
    pub point: f64,
    /// Lower bound of the credible interval.
    pub lower: f64,
    /// Upper bound of the credible interval.
    pub upper: f64,
}

impl ConfidenceInterval {
    /// Create a confidence interval, clamping to valid bounds.
    pub fn new(point: f64, lower: f64, upper: f64) -> Self {
        let p = point.clamp(0.0, 1.0);
        let lo = lower.clamp(0.0, p);
        let hi = upper.clamp(p, 1.0);
        Self {
            point: p,
            lower: lo,
            upper: hi,
        }
    }

    /// Create from a single point with no uncertainty (interval width = 0).
    pub fn exact(point: f64) -> Self {
        let p = point.clamp(0.0, 1.0);
        Self {
            point: p,
            lower: p,
            upper: p,
        }
    }

    /// Create from a point estimate with symmetric uncertainty margin.
    pub fn symmetric(point: f64, margin: f64) -> Self {
        let m = margin.abs();
        Self::new(point, point - m, point + m)
    }

    /// Zero confidence.
    pub const ZERO: Self = Self {
        point: 0.0,
        lower: 0.0,
        upper: 0.0,
    };

    /// Full confidence.
    pub const FULL: Self = Self {
        point: 1.0,
        lower: 1.0,
        upper: 1.0,
    };

    /// Width of the interval (measure of uncertainty).
    pub fn width(&self) -> f64 {
        self.upper - self.lower
    }

    /// Whether this is a high-confidence value (point ≥ 0.7, width ≤ 0.3).
    pub fn is_high(&self) -> bool {
        self.point >= 0.7 && self.width() <= 0.3
    }

    /// Whether the interval is wide enough to be considered uncertain (width > 0.4).
    pub fn is_uncertain(&self) -> bool {
        self.width() > 0.4
    }

    /// Whether the point estimate exceeds a threshold.
    pub fn exceeds(&self, threshold: f64) -> bool {
        self.point >= threshold
    }

    /// Conservative check: lower bound exceeds threshold.
    pub fn conservatively_exceeds(&self, threshold: f64) -> bool {
        self.lower >= threshold
    }

    /// Take the minimum of two intervals (element-wise min).
    pub fn min(self, other: Self) -> Self {
        Self {
            point: self.point.min(other.point),
            lower: self.lower.min(other.lower),
            upper: self.upper.min(other.upper),
        }
    }

    /// Merge two intervals by averaging points and widening bounds.
    pub fn merge(self, other: Self) -> Self {
        Self::new(
            (self.point + other.point) / 2.0,
            self.lower.min(other.lower),
            self.upper.max(other.upper),
        )
    }
}

impl Default for ConfidenceInterval {
    fn default() -> Self {
        Self::exact(0.0)
    }
}

impl PartialEq for ConfidenceInterval {
    fn eq(&self, other: &Self) -> bool {
        (self.point - other.point).abs() < f64::EPSILON
            && (self.lower - other.lower).abs() < f64::EPSILON
            && (self.upper - other.upper).abs() < f64::EPSILON
    }
}

impl std::fmt::Display for ConfidenceInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if (self.upper - self.lower).abs() < f64::EPSILON {
            write!(f, "{:.2}", self.point)
        } else {
            write!(
                f,
                "{:.2} [{:.2}, {:.2}]",
                self.point, self.lower, self.upper
            )
        }
    }
}

/// Allow `ConfidenceInterval` to be used where a bare `f64` threshold comparison
/// is needed (e.g., `if ci >= 0.5`).
impl PartialEq<f64> for ConfidenceInterval {
    fn eq(&self, other: &f64) -> bool {
        (self.point - other).abs() < f64::EPSILON
    }
}

impl PartialOrd<f64> for ConfidenceInterval {
    fn partial_cmp(&self, other: &f64) -> Option<std::cmp::Ordering> {
        self.point.partial_cmp(other)
    }
}

impl From<f64> for ConfidenceInterval {
    fn from(val: f64) -> Self {
        Self::exact(val)
    }
}

impl From<ConfidenceInterval> for f64 {
    fn from(ci: ConfidenceInterval) -> Self {
        ci.point
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_clamps_to_valid_bounds() {
        let ci = ConfidenceInterval::new(0.5, -0.1, 1.5);
        assert_eq!(ci.lower, 0.0);
        assert_eq!(ci.upper, 1.0);
        assert_eq!(ci.point, 0.5);
    }

    #[test]
    fn new_clamps_lower_to_point() {
        let ci = ConfidenceInterval::new(0.3, 0.8, 0.9);
        assert!(ci.lower <= ci.point);
    }

    #[test]
    fn new_clamps_upper_to_point() {
        let ci = ConfidenceInterval::new(0.9, 0.1, 0.5);
        assert!(ci.upper >= ci.point);
    }

    #[test]
    fn exact_has_zero_width() {
        let ci = ConfidenceInterval::exact(0.7);
        assert_eq!(ci.width(), 0.0);
        assert_eq!(ci.point, 0.7);
        assert_eq!(ci.lower, 0.7);
        assert_eq!(ci.upper, 0.7);
    }

    #[test]
    fn symmetric_creates_margin() {
        let ci = ConfidenceInterval::symmetric(0.5, 0.2);
        assert!((ci.point - 0.5).abs() < f64::EPSILON);
        assert!((ci.lower - 0.3).abs() < f64::EPSILON);
        assert!((ci.upper - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn symmetric_clamps_at_boundaries() {
        let ci = ConfidenceInterval::symmetric(0.9, 0.3);
        assert_eq!(ci.upper, 1.0);
        assert!((ci.lower - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn is_high_with_narrow_interval() {
        assert!(ConfidenceInterval::new(0.8, 0.7, 0.9).is_high());
        assert!(!ConfidenceInterval::new(0.5, 0.3, 0.7).is_high());
        assert!(!ConfidenceInterval::new(0.8, 0.2, 0.9).is_high());
    }

    #[test]
    fn is_uncertain_with_wide_interval() {
        assert!(ConfidenceInterval::new(0.5, 0.1, 0.9).is_uncertain());
        assert!(!ConfidenceInterval::new(0.5, 0.4, 0.6).is_uncertain());
    }

    #[test]
    fn exceeds_and_conservatively_exceeds() {
        let ci = ConfidenceInterval::new(0.7, 0.4, 0.9);
        assert!(ci.exceeds(0.5));
        assert!(!ci.conservatively_exceeds(0.5));
        assert!(ci.conservatively_exceeds(0.3));
    }

    #[test]
    fn min_takes_elementwise_minimum() {
        let a = ConfidenceInterval::new(0.8, 0.6, 0.9);
        let b = ConfidenceInterval::new(0.5, 0.3, 0.7);
        let m = a.min(b);
        assert!((m.point - 0.5).abs() < f64::EPSILON);
        assert!((m.lower - 0.3).abs() < f64::EPSILON);
        assert!((m.upper - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn merge_averages_point_widens_bounds() {
        let a = ConfidenceInterval::new(0.6, 0.5, 0.7);
        let b = ConfidenceInterval::new(0.8, 0.6, 0.9);
        let m = a.merge(b);
        assert!((m.point - 0.7).abs() < f64::EPSILON);
        assert!((m.lower - 0.5).abs() < f64::EPSILON);
        assert!((m.upper - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn from_f64_creates_exact() {
        let ci: ConfidenceInterval = 0.42.into();
        assert_eq!(ci, ConfidenceInterval::exact(0.42));
    }

    #[test]
    fn into_f64_extracts_point() {
        let ci = ConfidenceInterval::new(0.7, 0.5, 0.9);
        let val: f64 = ci.into();
        assert!((val - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn display_exact() {
        let ci = ConfidenceInterval::exact(0.85);
        assert_eq!(format!("{ci}"), "0.85");
    }

    #[test]
    fn display_interval() {
        let ci = ConfidenceInterval::new(0.7, 0.5, 0.9);
        assert_eq!(format!("{ci}"), "0.70 [0.50, 0.90]");
    }

    #[test]
    fn partial_ord_with_f64() {
        let ci = ConfidenceInterval::exact(0.7);
        assert!(ci >= 0.5);
        assert!(ci < 0.8);
    }

    #[test]
    fn serde_roundtrip() {
        let ci = ConfidenceInterval::new(0.7, 0.5, 0.9);
        let json = serde_json::to_string(&ci).unwrap();
        let parsed: ConfidenceInterval = serde_json::from_str(&json).unwrap();
        assert_eq!(ci, parsed);
    }

    #[test]
    fn constants() {
        assert_eq!(ConfidenceInterval::ZERO.point, 0.0);
        assert_eq!(ConfidenceInterval::FULL.point, 1.0);
    }

    #[test]
    fn default_is_zero() {
        assert_eq!(ConfidenceInterval::default(), ConfidenceInterval::ZERO);
    }
}
