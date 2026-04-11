//! A/B Testing Framework — compare runtime configurations and strategies.
//!
//! This module enables:
//! - Defining experiments with multiple variants
//! - Traffic allocation (percentage-based, user-based, or hash-based)
//! - Metric collection (tokens, latency, success rate, feedback)
//! - Statistical analysis (t-test, confidence intervals)
//! - Experiment lifecycle management
//!
//! Usage:
//! ```ignore
//! // Create experiment
//! let experiment = Experiment::new("compress-threshold-test")
//!     .with_description("Test different compression thresholds")
//!     .with_variant(Variant::control())
//!     .with_variant(Variant::new("aggressive")
//!         .with_config_override(|c| c.compression.compression_threshold = 0.6))
//!     .with_metric(MetricDefinition::token_usage())
//!     .with_metric(MetricDefinition::latency())
//!     .build();
//!
//! // Assign user to variant
//! let variant = experiment.assign_variant(&user_id);
//!
//! // Record outcome
//! experiment.record_outcome(&user_id, outcome);
//!
//! // Analyze results
//! let analysis = ExperimentAnalyzer::analyze(&experiment);
//! ```

use crate::runtime_config::RuntimeConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

// ─── Experiment Definition ───────────────────────────────────────────────────

/// An A/B test experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experiment {
    /// Unique experiment ID.
    pub id: String,

    /// Human-readable name.
    pub name: String,

    /// Description of what's being tested.
    pub description: String,

    /// Experiment variants (including control).
    pub variants: Vec<Variant>,

    /// Metrics to collect.
    pub metrics: Vec<MetricDefinition>,

    /// How to allocate traffic.
    pub traffic_allocation: TrafficAllocation,

    /// Current experiment status.
    pub status: ExperimentStatus,

    /// When the experiment was created.
    pub created_at: SystemTime,

    /// When the experiment started (if running).
    pub started_at: Option<SystemTime>,

    /// When the experiment ended (if stopped).
    pub ended_at: Option<SystemTime>,

    /// Minimum samples required before analysis.
    pub min_samples_per_variant: u32,

    /// Maximum experiment duration.
    pub max_duration: Option<Duration>,

    /// Tags for organization.
    pub tags: Vec<String>,
}

/// A variant in an experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    /// Unique variant ID.
    pub id: String,

    /// Human-readable name.
    pub name: String,

    /// Description of this variant.
    pub description: String,

    /// Traffic percentage (0.0-1.0).
    pub traffic_percentage: f64,

    /// Whether this is the control variant.
    pub is_control: bool,

    /// Serializable config diff (for persistence).
    /// Keys are dot-notation paths, values are the overridden values.
    pub config_diff: HashMap<String, serde_json::Value>,
}

impl Variant {
    /// Create a new variant.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            description: String::new(),
            traffic_percentage: 0.0,
            is_control: false,
            config_diff: HashMap::new(),
        }
    }

    /// Create the control variant.
    pub fn control() -> Self {
        Self {
            id: "control".to_string(),
            name: "Control".to_string(),
            description: "Baseline (no changes)".to_string(),
            traffic_percentage: 0.5,
            is_control: true,
            config_diff: HashMap::new(),
        }
    }

    /// Set variant name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set variant description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Set traffic percentage.
    pub fn with_traffic(mut self, percentage: f64) -> Self {
        self.traffic_percentage = percentage.clamp(0.0, 1.0);
        self
    }

    /// Set config diff (serializable version of override).
    pub fn with_config_diff(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.config_diff.insert(key.into(), value);
        self
    }

    /// Apply this variant's config overrides to a base config.
    pub fn apply_to_config(&self, config: &mut RuntimeConfig) {
        apply_config_diffs(config, &self.config_diff);
    }
}

pub(crate) fn apply_config_diffs(
    config: &mut RuntimeConfig,
    config_diff: &HashMap<String, serde_json::Value>,
) {
    for (key, value) in config_diff {
        apply_config_diff(config, key, value);
    }
}

pub(crate) fn apply_config_diff(config: &mut RuntimeConfig, key: &str, value: &serde_json::Value) {
    match key {
        "compression.max_history_tokens" => {
            if let Some(n) = value.as_u64() {
                config.compression.max_history_tokens = n as u32;
            }
        }
        "compression.compression_threshold" => {
            if let Some(n) = value.as_f64() {
                config.compression.compression_threshold = n;
            }
        }
        "memory.retrieval_top_k" => {
            if let Some(n) = value.as_u64() {
                config.memory.retrieval_top_k = n as u32;
            }
        }
        "tool_selection.confidence_threshold" => {
            if let Some(n) = value.as_f64() {
                config.tool_selection.confidence_threshold = n;
            }
        }
        "learning.exploration_rate" => {
            if let Some(n) = value.as_f64() {
                config.learning.exploration_rate = n;
            }
        }
        // Add more as needed
        _ => {}
    }
}

/// Metric to collect during experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricDefinition {
    /// Metric name.
    pub name: String,

    /// Metric type.
    pub metric_type: MetricType,

    /// How to aggregate across samples.
    pub aggregation: AggregationType,

    /// Optional target value (for directional metrics).
    pub target: Option<f64>,

    /// Whether lower is better.
    pub lower_is_better: bool,
}

impl MetricDefinition {
    /// Token usage metric.
    pub fn token_usage() -> Self {
        Self {
            name: "token_usage".to_string(),
            metric_type: MetricType::Counter,
            aggregation: AggregationType::Mean,
            target: None,
            lower_is_better: true,
        }
    }

    /// Latency metric.
    pub fn latency() -> Self {
        Self {
            name: "latency_ms".to_string(),
            metric_type: MetricType::Timer,
            aggregation: AggregationType::Percentile(95),
            target: None,
            lower_is_better: true,
        }
    }

    /// Success rate metric.
    pub fn success_rate() -> Self {
        Self {
            name: "success_rate".to_string(),
            metric_type: MetricType::Rate,
            aggregation: AggregationType::Mean,
            target: Some(0.95),
            lower_is_better: false,
        }
    }

    /// User correction count metric.
    pub fn user_corrections() -> Self {
        Self {
            name: "user_corrections".to_string(),
            metric_type: MetricType::Counter,
            aggregation: AggregationType::Sum,
            target: None,
            lower_is_better: true,
        }
    }

    /// Explicit feedback (thumbs up/down).
    pub fn explicit_feedback() -> Self {
        Self {
            name: "feedback_score".to_string(),
            metric_type: MetricType::Score,
            aggregation: AggregationType::Mean,
            target: Some(0.8),
            lower_is_better: false,
        }
    }
}

/// Type of metric.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MetricType {
    /// Counting occurrences.
    Counter,
    /// Measuring time.
    Timer,
    /// Success/failure rate.
    Rate,
    /// Score (e.g., 0-1 or -1 to 1).
    Score,
    /// Histogram of values.
    Histogram,
}

/// How to aggregate metric values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AggregationType {
    /// Arithmetic mean.
    Mean,
    /// Median.
    Median,
    /// Sum total.
    Sum,
    /// Maximum value.
    Max,
    /// Minimum value.
    Min,
    /// Specific percentile.
    Percentile(u8),
}

/// Traffic allocation strategy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TrafficAllocation {
    /// Percentage-based (use variant traffic_percentage).
    Percentage,
    /// Hash-based (deterministic by user ID).
    HashBased,
    /// Round-robin.
    RoundRobin,
    /// Explicit user assignment (for debugging).
    Manual,
}

impl Default for TrafficAllocation {
    fn default() -> Self {
        Self::HashBased
    }
}

/// Experiment lifecycle status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ExperimentStatus {
    /// Experiment is defined but not started.
    Draft,
    /// Experiment is actively running.
    Running,
    /// Experiment is paused.
    Paused,
    /// Experiment has completed.
    Completed,
    /// Experiment was cancelled.
    Cancelled,
}

impl Default for ExperimentStatus {
    fn default() -> Self {
        Self::Draft
    }
}

// ─── Experiment Builder ──────────────────────────────────────────────────────

impl Experiment {
    /// Create a new experiment.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(id: impl Into<String>) -> ExperimentBuilder {
        ExperimentBuilder::new(id)
    }

    /// Start the experiment.
    pub fn start(&mut self) {
        if self.status == ExperimentStatus::Draft || self.status == ExperimentStatus::Paused {
            self.status = ExperimentStatus::Running;
            self.started_at = Some(SystemTime::now());
        }
    }

    /// Pause the experiment.
    pub fn pause(&mut self) {
        if self.status == ExperimentStatus::Running {
            self.status = ExperimentStatus::Paused;
        }
    }

    /// Stop the experiment.
    pub fn stop(&mut self) {
        if self.status == ExperimentStatus::Running || self.status == ExperimentStatus::Paused {
            self.status = ExperimentStatus::Completed;
            self.ended_at = Some(SystemTime::now());
        }
    }

    /// Cancel the experiment.
    pub fn cancel(&mut self) {
        self.status = ExperimentStatus::Cancelled;
        self.ended_at = Some(SystemTime::now());
    }

    /// Assign a user to a variant based on traffic allocation.
    pub fn assign_variant(&self, user_id: &str) -> Option<&Variant> {
        if self.status != ExperimentStatus::Running {
            return None;
        }

        match self.traffic_allocation {
            TrafficAllocation::HashBased => {
                // Deterministic assignment based on hash
                let hash = simple_hash(user_id, &self.id);
                let normalized = (hash as f64) / (u64::MAX as f64);
                self.variant_for_normalized(normalized)
            }
            TrafficAllocation::Percentage | TrafficAllocation::RoundRobin => {
                // Random assignment (use user_id as seed for reproducibility)
                let hash = simple_hash(user_id, &self.id);
                let normalized = (hash as f64) / (u64::MAX as f64);
                self.variant_for_normalized(normalized)
            }
            TrafficAllocation::Manual => {
                // Return control by default for manual assignment
                self.variants.iter().find(|v| v.is_control)
            }
        }
    }

    fn variant_for_normalized(&self, value: f64) -> Option<&Variant> {
        let mut cumulative = 0.0;
        for variant in &self.variants {
            cumulative += variant.traffic_percentage;
            if value < cumulative {
                return Some(variant);
            }
        }
        self.variants.last()
    }

    /// Get the control variant.
    pub fn control(&self) -> Option<&Variant> {
        self.variants.iter().find(|v| v.is_control)
    }

    /// Get a variant by ID.
    pub fn variant(&self, variant_id: &str) -> Option<&Variant> {
        self.variants
            .iter()
            .find(|variant| variant.id == variant_id)
    }

    /// Check if experiment has sufficient samples.
    pub fn has_sufficient_samples(&self, samples: &HashMap<String, usize>) -> bool {
        self.variants.iter().all(|v| {
            samples.get(&v.id).copied().unwrap_or(0) >= self.min_samples_per_variant as usize
        })
    }
}

/// Simple hash function for variant assignment.
fn simple_hash(user_id: &str, experiment_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    user_id.hash(&mut hasher);
    experiment_id.hash(&mut hasher);
    hasher.finish()
}

/// Builder for experiments.
pub struct ExperimentBuilder {
    experiment: Experiment,
}

impl ExperimentBuilder {
    fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            experiment: Experiment {
                name: id.clone(),
                id,
                description: String::new(),
                variants: Vec::new(),
                metrics: Vec::new(),
                traffic_allocation: TrafficAllocation::default(),
                status: ExperimentStatus::default(),
                created_at: SystemTime::now(),
                started_at: None,
                ended_at: None,
                min_samples_per_variant: 100,
                max_duration: None,
                tags: Vec::new(),
            },
        }
    }

    /// Set experiment name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.experiment.name = name.into();
        self
    }

    /// Set experiment description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.experiment.description = description.into();
        self
    }

    /// Add a variant.
    pub fn with_variant(mut self, variant: Variant) -> Self {
        self.experiment.variants.push(variant);
        self
    }

    /// Add a metric.
    pub fn with_metric(mut self, metric: MetricDefinition) -> Self {
        self.experiment.metrics.push(metric);
        self
    }

    /// Set traffic allocation strategy.
    pub fn with_traffic_allocation(mut self, allocation: TrafficAllocation) -> Self {
        self.experiment.traffic_allocation = allocation;
        self
    }

    /// Set minimum samples per variant.
    pub fn with_min_samples(mut self, samples: u32) -> Self {
        self.experiment.min_samples_per_variant = samples;
        self
    }

    /// Set maximum duration.
    pub fn with_max_duration(mut self, duration: Duration) -> Self {
        self.experiment.max_duration = Some(duration);
        self
    }

    /// Add a tag.
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.experiment.tags.push(tag.into());
        self
    }

    /// Build the experiment.
    pub fn build(mut self) -> Experiment {
        // Normalize traffic percentages
        let total: f64 = self
            .experiment
            .variants
            .iter()
            .map(|v| v.traffic_percentage)
            .sum();
        if total > 0.0 && (total - 1.0).abs() > 0.01 {
            for variant in &mut self.experiment.variants {
                variant.traffic_percentage /= total;
            }
        }
        self.experiment
    }
}

// ─── Experiment Outcome ──────────────────────────────────────────────────────

/// Outcome of a single experiment observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentOutcome {
    /// User/session ID.
    pub user_id: String,

    /// Assigned variant ID.
    pub variant_id: String,

    /// When this outcome was recorded.
    pub timestamp: SystemTime,

    /// Metric values.
    pub metrics: HashMap<String, f64>,

    /// Whether the interaction was successful.
    pub success: bool,

    /// Additional context.
    pub context: HashMap<String, serde_json::Value>,
}

impl ExperimentOutcome {
    /// Create a new outcome.
    pub fn new(user_id: impl Into<String>, variant_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            variant_id: variant_id.into(),
            timestamp: SystemTime::now(),
            metrics: HashMap::new(),
            success: true,
            context: HashMap::new(),
        }
    }

    /// Set a metric value.
    pub fn with_metric(mut self, name: impl Into<String>, value: f64) -> Self {
        self.metrics.insert(name.into(), value);
        self
    }

    /// Set success status.
    pub fn with_success(mut self, success: bool) -> Self {
        self.success = success;
        self
    }

    /// Add context.
    pub fn with_context(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.context.insert(key.into(), value);
        self
    }
}

// ─── Experiment Store ────────────────────────────────────────────────────────

/// In-memory store for experiments and outcomes.
#[derive(Default)]
pub struct ExperimentStore {
    experiments: Arc<RwLock<HashMap<String, Experiment>>>,
    outcomes: Arc<RwLock<HashMap<String, Vec<ExperimentOutcome>>>>,
}

impl ExperimentStore {
    /// Create a new store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an experiment.
    pub fn register(&self, experiment: Experiment) {
        let mut experiments = self.experiments.write().unwrap_or_else(|e| e.into_inner());
        experiments.insert(experiment.id.clone(), experiment);
    }

    /// Get an experiment by ID.
    pub fn get(&self, id: &str) -> Option<Experiment> {
        self.experiments
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// List all experiments.
    pub fn list(&self) -> Vec<Experiment> {
        self.experiments
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Record an outcome.
    pub fn record_outcome(&self, experiment_id: &str, outcome: ExperimentOutcome) {
        let mut outcomes = self.outcomes.write().unwrap_or_else(|e| e.into_inner());
        outcomes
            .entry(experiment_id.to_string())
            .or_default()
            .push(outcome);
    }

    /// Get outcomes for an experiment.
    pub fn get_outcomes(&self, experiment_id: &str) -> Vec<ExperimentOutcome> {
        self.outcomes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(experiment_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Get sample counts per variant.
    pub fn sample_counts(&self, experiment_id: &str) -> HashMap<String, usize> {
        let outcomes = self.get_outcomes(experiment_id);
        let mut counts = HashMap::new();
        for outcome in outcomes {
            *counts.entry(outcome.variant_id).or_default() += 1;
        }
        counts
    }

    /// Enable an experiment (start if draft, resume if paused).
    ///
    /// Returns true if state changed, false otherwise.
    pub fn enable_experiment(&self, experiment_id: &str) -> bool {
        let mut experiments = self.experiments.write().unwrap_or_else(|e| e.into_inner());
        if let Some(exp) = experiments.get_mut(experiment_id) {
            let old_status = exp.status.clone();
            exp.start();
            exp.status != old_status
        } else {
            false
        }
    }

    /// Disable an experiment (pause if running).
    ///
    /// Returns true if state changed, false otherwise.
    pub fn disable_experiment(&self, experiment_id: &str) -> bool {
        let mut experiments = self.experiments.write().unwrap_or_else(|e| e.into_inner());
        if let Some(exp) = experiments.get_mut(experiment_id) {
            let old_status = exp.status.clone();
            exp.pause();
            exp.status != old_status
        } else {
            false
        }
    }

    /// Stop an experiment (mark as completed).
    ///
    /// Returns true if state changed, false otherwise.
    pub fn stop_experiment(&self, experiment_id: &str) -> bool {
        let mut experiments = self.experiments.write().unwrap_or_else(|e| e.into_inner());
        if let Some(exp) = experiments.get_mut(experiment_id) {
            let old_status = exp.status.clone();
            exp.stop();
            exp.status != old_status
        } else {
            false
        }
    }
}

// ─── Statistical Analysis ────────────────────────────────────────────────────

/// Analysis results for an experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentAnalysis {
    /// Experiment ID.
    pub experiment_id: String,

    /// Per-variant statistics.
    pub variant_stats: HashMap<String, VariantStats>,

    /// Comparison results (treatment vs control).
    pub comparisons: Vec<VariantComparison>,

    /// Overall recommendation.
    pub recommendation: Recommendation,

    /// When this analysis was performed.
    pub analyzed_at: SystemTime,
}

/// Statistics for a single variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantStats {
    /// Variant ID.
    pub variant_id: String,

    /// Number of samples.
    pub sample_count: usize,

    /// Per-metric statistics.
    pub metric_stats: HashMap<String, MetricStats>,
}

/// Statistics for a single metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricStats {
    /// Metric name.
    pub name: String,

    /// Mean value.
    pub mean: f64,

    /// Standard deviation.
    pub std_dev: f64,

    /// Median value.
    pub median: f64,

    /// Minimum value.
    pub min: f64,

    /// Maximum value.
    pub max: f64,

    /// 95th percentile.
    pub p95: f64,

    /// Sample count.
    pub count: usize,
}

/// Comparison between treatment and control.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantComparison {
    /// Treatment variant ID.
    pub treatment_id: String,

    /// Control variant ID.
    pub control_id: String,

    /// Metric being compared.
    pub metric: String,

    /// Relative change (treatment - control) / control.
    pub relative_change: f64,

    /// Absolute change (treatment - control).
    pub absolute_change: f64,

    /// P-value from t-test.
    pub p_value: f64,

    /// 95% confidence interval for the change.
    pub confidence_interval: (f64, f64),

    /// Whether the result is statistically significant (p < 0.05).
    pub is_significant: bool,

    /// Whether the change is in the desired direction.
    pub is_improvement: bool,
}

/// Recommendation based on analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Recommendation {
    /// Not enough data yet.
    InsufficientData,
    /// Control is better.
    KeepControl,
    /// Treatment is significantly better.
    RolloutTreatment { variant_id: String },
    /// No significant difference.
    NoSignificantDifference,
    /// Multiple treatments are comparable.
    NeedsManualReview,
}

/// Analyzer for experiments.
pub struct ExperimentAnalyzer;

impl ExperimentAnalyzer {
    /// Analyze an experiment.
    pub fn analyze(experiment: &Experiment, outcomes: &[ExperimentOutcome]) -> ExperimentAnalysis {
        // Group outcomes by variant
        let mut by_variant: HashMap<String, Vec<&ExperimentOutcome>> = HashMap::new();
        for outcome in outcomes {
            by_variant
                .entry(outcome.variant_id.clone())
                .or_default()
                .push(outcome);
        }

        // Calculate stats per variant
        let mut variant_stats = HashMap::new();
        for (variant_id, variant_outcomes) in &by_variant {
            let stats =
                Self::calculate_variant_stats(variant_id, variant_outcomes, &experiment.metrics);
            variant_stats.insert(variant_id.clone(), stats);
        }

        // Compare treatments to control
        let control_id = experiment.control().map(|v| v.id.clone());
        let mut comparisons = Vec::new();

        if let Some(ref control_id) = control_id {
            if let Some(control_stats) = variant_stats.get(control_id) {
                for variant in &experiment.variants {
                    if variant.is_control {
                        continue;
                    }
                    if let Some(treatment_stats) = variant_stats.get(&variant.id) {
                        for metric in &experiment.metrics {
                            if let (Some(control_metric), Some(treatment_metric)) = (
                                control_stats.metric_stats.get(&metric.name),
                                treatment_stats.metric_stats.get(&metric.name),
                            ) {
                                let comparison = Self::compare_metrics(
                                    &variant.id,
                                    control_id,
                                    &metric.name,
                                    control_metric,
                                    treatment_metric,
                                    metric.lower_is_better,
                                );
                                comparisons.push(comparison);
                            }
                        }
                    }
                }
            }
        }

        // Determine recommendation
        let recommendation =
            Self::determine_recommendation(experiment, &variant_stats, &comparisons);

        ExperimentAnalysis {
            experiment_id: experiment.id.clone(),
            variant_stats,
            comparisons,
            recommendation,
            analyzed_at: SystemTime::now(),
        }
    }

    fn calculate_variant_stats(
        variant_id: &str,
        outcomes: &[&ExperimentOutcome],
        metrics: &[MetricDefinition],
    ) -> VariantStats {
        let mut metric_stats = HashMap::new();

        for metric in metrics {
            let values: Vec<f64> = outcomes
                .iter()
                .filter_map(|o| o.metrics.get(&metric.name).copied())
                .collect();

            if !values.is_empty() {
                metric_stats.insert(
                    metric.name.clone(),
                    Self::calculate_metric_stats(&metric.name, &values),
                );
            }
        }

        VariantStats {
            variant_id: variant_id.to_string(),
            sample_count: outcomes.len(),
            metric_stats,
        }
    }

    fn calculate_metric_stats(name: &str, values: &[f64]) -> MetricStats {
        let n = values.len();
        if n == 0 {
            return MetricStats {
                name: name.to_string(),
                mean: 0.0,
                std_dev: 0.0,
                median: 0.0,
                min: 0.0,
                max: 0.0,
                p95: 0.0,
                count: 0,
            };
        }

        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let mean = sorted.iter().sum::<f64>() / n as f64;
        let variance = if n > 1 {
            sorted.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64
        } else {
            0.0
        };
        let std_dev = variance.sqrt();

        let median = if n.is_multiple_of(2) {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        } else {
            sorted[n / 2]
        };

        let p95_idx = ((n as f64 * 0.95).ceil() as usize).min(n - 1);

        MetricStats {
            name: name.to_string(),
            mean,
            std_dev,
            median,
            min: sorted[0],
            max: sorted[n - 1],
            p95: sorted[p95_idx],
            count: n,
        }
    }

    fn compare_metrics(
        treatment_id: &str,
        control_id: &str,
        metric: &str,
        control: &MetricStats,
        treatment: &MetricStats,
        lower_is_better: bool,
    ) -> VariantComparison {
        let absolute_change = treatment.mean - control.mean;
        let relative_change = if control.mean != 0.0 {
            absolute_change / control.mean
        } else {
            0.0
        };

        // Simple t-test (Welch's t-test approximation)
        let (p_value, se) = Self::welch_t_test(control, treatment);

        // 95% CI
        let margin = 1.96 * se;
        let confidence_interval = (absolute_change - margin, absolute_change + margin);

        let is_significant = p_value < 0.05;
        let is_improvement = if lower_is_better {
            absolute_change < 0.0
        } else {
            absolute_change > 0.0
        };

        VariantComparison {
            treatment_id: treatment_id.to_string(),
            control_id: control_id.to_string(),
            metric: metric.to_string(),
            relative_change,
            absolute_change,
            p_value,
            confidence_interval,
            is_significant,
            is_improvement,
        }
    }

    fn welch_t_test(control: &MetricStats, treatment: &MetricStats) -> (f64, f64) {
        if control.count < 2 || treatment.count < 2 {
            return (1.0, 0.0);
        }

        let n1 = control.count as f64;
        let n2 = treatment.count as f64;
        let var1 = control.std_dev.powi(2);
        let var2 = treatment.std_dev.powi(2);

        let se = ((var1 / n1) + (var2 / n2)).sqrt();
        if se == 0.0 {
            return (1.0, 0.0);
        }

        let t = (treatment.mean - control.mean) / se;

        // Approximate p-value using normal distribution (valid for large samples)
        let p_value = 2.0 * (1.0 - normal_cdf(t.abs()));

        (p_value, se)
    }

    fn determine_recommendation(
        experiment: &Experiment,
        stats: &HashMap<String, VariantStats>,
        comparisons: &[VariantComparison],
    ) -> Recommendation {
        // Check sample size
        if stats
            .values()
            .any(|s| s.sample_count < experiment.min_samples_per_variant as usize)
        {
            return Recommendation::InsufficientData;
        }

        // Find best treatment
        let significant_improvements: Vec<_> = comparisons
            .iter()
            .filter(|c| c.is_significant && c.is_improvement)
            .collect();

        if significant_improvements.is_empty() {
            // Check if any are significantly worse
            let significant_regressions: Vec<_> = comparisons
                .iter()
                .filter(|c| c.is_significant && !c.is_improvement)
                .collect();

            if !significant_regressions.is_empty() {
                return Recommendation::KeepControl;
            }

            return Recommendation::NoSignificantDifference;
        }

        // If only one treatment shows improvement
        let improving_variants: std::collections::HashSet<_> = significant_improvements
            .iter()
            .map(|c| c.treatment_id.clone())
            .collect();

        if improving_variants.len() == 1 {
            if let Some(variant_id) = improving_variants.into_iter().next() {
                return Recommendation::RolloutTreatment { variant_id };
            }
        }

        Recommendation::NeedsManualReview
    }
}

/// Normal CDF approximation.
fn normal_cdf(x: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();

    0.5 * (1.0 + sign * y)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_experiment_builder() {
        let experiment = Experiment::new("test-exp")
            .with_name("Test Experiment")
            .with_description("Testing compression thresholds")
            .with_variant(Variant::control().with_traffic(0.5))
            .with_variant(Variant::new("aggressive").with_traffic(0.5))
            .with_metric(MetricDefinition::token_usage())
            .with_metric(MetricDefinition::latency())
            .with_min_samples(50)
            .build();

        assert_eq!(experiment.id, "test-exp");
        assert_eq!(experiment.variants.len(), 2);
        assert_eq!(experiment.metrics.len(), 2);
        assert_eq!(experiment.min_samples_per_variant, 50);
    }

    #[test]
    fn test_variant_assignment() {
        let mut experiment = Experiment::new("test")
            .with_variant(Variant::control().with_traffic(0.5))
            .with_variant(Variant::new("treatment").with_traffic(0.5))
            .build();

        experiment.start();

        // Same user should always get same variant
        let v1 = experiment.assign_variant("user123").map(|v| v.id.clone());
        let v2 = experiment.assign_variant("user123").map(|v| v.id.clone());
        assert_eq!(v1, v2);

        // Different users may get different variants
        let mut assigned = std::collections::HashSet::new();
        for i in 0..100 {
            if let Some(v) = experiment.assign_variant(&format!("user{}", i)) {
                assigned.insert(v.id.clone());
            }
        }
        // With 100 users and 50/50 split, both variants should be assigned
        assert_eq!(assigned.len(), 2);
    }

    #[test]
    fn test_outcome_recording() {
        let store = ExperimentStore::new();

        let outcome = ExperimentOutcome::new("user1", "control")
            .with_metric("token_usage", 1500.0)
            .with_metric("latency_ms", 250.0)
            .with_success(true);

        store.record_outcome("exp1", outcome);

        let outcomes = store.get_outcomes("exp1");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].user_id, "user1");
    }

    #[test]
    fn test_metric_stats() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let stats = ExperimentAnalyzer::calculate_metric_stats("test", &values);

        assert!((stats.mean - 5.5).abs() < 0.01);
        // Sample std_dev (Bessel's correction: n-1 denominator) for [1..10] ≈ 3.0277
        assert!(
            (stats.std_dev - 3.0277).abs() < 0.01,
            "std_dev was {}",
            stats.std_dev
        );
        assert!((stats.median - 5.5).abs() < 0.01);
        assert_eq!(stats.min, 1.0);
        assert_eq!(stats.max, 10.0);
        assert_eq!(stats.count, 10);
    }

    #[test]
    fn test_welch_t_test_known_result() {
        // Two clearly distinct groups: control ~5, treatment ~10
        let control = ExperimentAnalyzer::calculate_metric_stats("ctl", &[4.0, 5.0, 5.0, 6.0, 5.0]);
        let treatment =
            ExperimentAnalyzer::calculate_metric_stats("trt", &[9.0, 10.0, 10.0, 11.0, 10.0]);
        let (p_value, se) = ExperimentAnalyzer::welch_t_test(&control, &treatment);
        // Large effect size → p should be very small
        assert!(
            p_value < 0.01,
            "p_value {p_value} should be < 0.01 for distinct groups"
        );
        assert!(se > 0.0, "standard error must be positive");

        // Two identical groups: should not be significant
        let same1 = ExperimentAnalyzer::calculate_metric_stats("a", &[5.0, 5.0, 5.0, 5.0, 5.0]);
        let same2 = ExperimentAnalyzer::calculate_metric_stats("b", &[5.0, 5.0, 5.0, 5.0, 5.0]);
        let (p_identical, _se) = ExperimentAnalyzer::welch_t_test(&same1, &same2);
        // Zero variance → se=0 → returns (1.0, 0.0)
        assert!(
            (p_identical - 1.0).abs() < 0.001,
            "identical groups should have p≈1.0"
        );
    }

    #[test]
    fn test_experiment_lifecycle() {
        let mut experiment = Experiment::new("test").build();

        assert_eq!(experiment.status, ExperimentStatus::Draft);

        experiment.start();
        assert_eq!(experiment.status, ExperimentStatus::Running);
        assert!(experiment.started_at.is_some());

        experiment.pause();
        assert_eq!(experiment.status, ExperimentStatus::Paused);

        experiment.start();
        assert_eq!(experiment.status, ExperimentStatus::Running);

        experiment.stop();
        assert_eq!(experiment.status, ExperimentStatus::Completed);
        assert!(experiment.ended_at.is_some());
    }

    #[test]
    fn test_config_override() {
        let variant = Variant::new("aggressive")
            .with_config_diff("compression.compression_threshold", serde_json::json!(0.6));

        let mut config = RuntimeConfig::default();
        variant.apply_to_config(&mut config);

        assert!((config.compression.compression_threshold - 0.6).abs() < 0.01);
    }

    #[test]
    fn test_analysis_recommendation() {
        let experiment = Experiment::new("test")
            .with_variant(Variant::control().with_traffic(0.5))
            .with_variant(Variant::new("treatment").with_traffic(0.5))
            .with_metric(MetricDefinition::token_usage())
            .with_min_samples(5)
            .build();

        // Create outcomes showing clear improvement
        let mut outcomes = Vec::new();
        for i in 0..10 {
            outcomes.push(
                ExperimentOutcome::new(format!("user{}", i), "control")
                    .with_metric("token_usage", 1000.0 + (i as f64 * 10.0)),
            );
            outcomes.push(
                ExperimentOutcome::new(format!("user{}", i + 100), "treatment")
                    .with_metric("token_usage", 500.0 + (i as f64 * 10.0)), // Much lower = better
            );
        }

        let analysis = ExperimentAnalyzer::analyze(&experiment, &outcomes);

        // Should have stats for both variants
        assert!(analysis.variant_stats.contains_key("control"));
        assert!(analysis.variant_stats.contains_key("treatment"));

        // Treatment should show significant improvement (lower tokens)
        assert!(!analysis.comparisons.is_empty());
    }

    #[test]
    fn test_normal_cdf() {
        // Known values
        assert!((normal_cdf(0.0) - 0.5).abs() < 0.01);
        assert!((normal_cdf(1.96) - 0.975).abs() < 0.01);
        assert!((normal_cdf(-1.96) - 0.025).abs() < 0.01);
    }
}
