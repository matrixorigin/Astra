/// Provides drift-score information for evolution triggers.
///
/// Implemented by any source that tracks "confidence drop" against a
/// historical baseline — the auto-tuner consumes it to drive
/// `PatternDrift` evolution rules. No in-tree implementation currently
/// supplies drift data; callers pass `None`.
pub trait DriftSource {
    /// Maximum drift score across all tracked entities (0.0 = no drift).
    fn max_drift_score(&self) -> f64;
}
