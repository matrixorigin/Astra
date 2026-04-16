/// Provides drift-score information for evolution triggers.
///
/// Runtime implements this for `PatternLibrary`, decoupling
/// auto-tuning from the pipeline subsystem.
pub trait DriftSource {
    /// Maximum drift score across all tracked patterns (0.0 = no drift).
    fn max_drift_score(&self) -> f64;
}
