//! Lightweight, zero-dependency Prometheus-compatible metrics registry.
//!
//! Design goals:
//! * **No new deps** — uses only std; keeps `astra-turn-core`'s dependency surface small.
//! * **Cheap hot-path updates** — counters/gauges use atomics, no locking on increment.
//! * **Deterministic output** — metric families and label sets rendered in sorted order.
//! * **Prometheus text exposition format 0.0.4** compatible.
//!
//! ## Scope
//!
//! This is a minimal in-process registry intended to back a `/metrics` HTTP
//! endpoint for Phase 13 observability. It intentionally does NOT implement
//! histograms, summaries, or exemplars — those can be layered on later via the
//! `metrics` crate if richer instrumentation becomes necessary.
//!
//! ## Concurrency
//!
//! The registry uses `RwLock` only for **registering new series**. Incrementing
//! existing counters / setting existing gauges takes only a read lock plus an
//! atomic op, which is safe for high-frequency hot paths.

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The kind of a metric family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
}

impl MetricKind {
    fn as_str(&self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
        }
    }
}

/// A canonical label set: sorted `(name, value)` pairs. Sorting is what makes
/// the rendered output deterministic and lets us use the labels as a map key.
type LabelSet = Vec<(String, String)>;

/// Storage for one metric family (all series sharing a name).
#[derive(Debug)]
struct MetricFamily {
    kind: MetricKind,
    help: String,
    /// For counters we store u64 bits; for gauges we store f64 bits via `to_bits`.
    series: BTreeMap<LabelSet, AtomicU64>,
}

/// In-process Prometheus-compatible metrics registry.
///
/// Clone-cheap (wraps an `RwLock`); in production you'd typically hold a single
/// `Arc<MetricsRegistry>` in `AppState` and share it across handlers.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    inner: RwLock<BTreeMap<String, MetricFamily>>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a counter. Idempotent — re-registering with the same name is a
    /// no-op (help text from the first registration wins).
    pub fn register_counter(&self, name: &str, help: &str) {
        self.register(name, help, MetricKind::Counter);
    }

    /// Register a gauge. Idempotent.
    pub fn register_gauge(&self, name: &str, help: &str) {
        self.register(name, help, MetricKind::Gauge);
    }

    fn register(&self, name: &str, help: &str, kind: MetricKind) {
        let mut guard = self.inner.write().expect("metrics registry poisoned");
        guard
            .entry(name.to_string())
            .or_insert_with(|| MetricFamily {
                kind,
                help: help.to_string(),
                series: BTreeMap::new(),
            });
    }

    /// Returns the registered kind for a metric, or `None` if unknown.
    pub fn kind_of(&self, name: &str) -> Option<MetricKind> {
        let guard = self.inner.read().expect("metrics registry poisoned");
        guard.get(name).map(|f| f.kind)
    }

    /// Increment a counter series by `delta`. Unknown metrics are silently
    /// ignored to keep hot paths panic-free.
    pub fn increment_counter(&self, name: &str, labels: &[(&str, &str)], delta: u64) {
        let labels = canonical_labels(labels);
        let mut guard = self.inner.write().expect("metrics registry poisoned");
        let Some(family) = guard.get_mut(name) else {
            return;
        };
        if family.kind != MetricKind::Counter {
            return;
        }
        let cell = family
            .series
            .entry(labels)
            .or_insert_with(|| AtomicU64::new(0));
        cell.fetch_add(delta, Ordering::Relaxed);
    }

    /// Set a gauge series to an absolute value. Unknown metrics are ignored.
    pub fn set_gauge(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        let labels = canonical_labels(labels);
        let mut guard = self.inner.write().expect("metrics registry poisoned");
        let Some(family) = guard.get_mut(name) else {
            return;
        };
        if family.kind != MetricKind::Gauge {
            return;
        }
        let cell = family
            .series
            .entry(labels)
            .or_insert_with(|| AtomicU64::new(0));
        cell.store(value.to_bits(), Ordering::Relaxed);
    }

    /// Render the full registry in Prometheus text exposition format.
    ///
    /// Output is sorted (by metric name, then by label set) and contains
    /// `# HELP` / `# TYPE` header lines per metric family.
    pub fn render_prometheus(&self) -> String {
        let guard = self.inner.read().expect("metrics registry poisoned");
        let mut out = String::new();
        for (name, family) in guard.iter() {
            out.push_str("# HELP ");
            out.push_str(name);
            out.push(' ');
            out.push_str(&family.help);
            out.push('\n');
            out.push_str("# TYPE ");
            out.push_str(name);
            out.push(' ');
            out.push_str(family.kind.as_str());
            out.push('\n');
            for (labels, cell) in family.series.iter() {
                out.push_str(name);
                if !labels.is_empty() {
                    out.push('{');
                    let mut first = true;
                    for (k, v) in labels {
                        if !first {
                            out.push(',');
                        }
                        first = false;
                        out.push_str(k);
                        out.push_str("=\"");
                        escape_label_value(v, &mut out);
                        out.push('"');
                    }
                    out.push('}');
                }
                out.push(' ');
                match family.kind {
                    MetricKind::Counter => {
                        let v = cell.load(Ordering::Relaxed);
                        out.push_str(&v.to_string());
                    }
                    MetricKind::Gauge => {
                        let bits = cell.load(Ordering::Relaxed);
                        let v = f64::from_bits(bits);
                        out.push_str(&format_gauge(v));
                    }
                }
                out.push('\n');
            }
        }
        out
    }
}

fn canonical_labels(labels: &[(&str, &str)]) -> LabelSet {
    let mut v: LabelSet = labels
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// Escape a label value per Prometheus text format:
/// `\` → `\\`, `"` → `\"`, `\n` → `\n`.
fn escape_label_value(value: &str, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str(r"\n"),
            other => out.push(other),
        }
    }
}

/// Format a gauge value. Whole numbers render without a decimal point so
/// `1.0` becomes `1`, matching what Prometheus clients typically emit.
fn format_gauge(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    } else if v.fract() == 0.0 && v.abs() < 1e16 {
        format!("{}", v as i64)
    } else {
        // strip trailing zeros from default float repr where possible
        let s = format!("{v}");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_escape_handles_quotes_backslash_newline() {
        let mut s = String::new();
        escape_label_value("a\"b\\c\nd", &mut s);
        assert_eq!(s, r#"a\"b\\c\nd"#);
    }

    #[test]
    fn gauge_whole_number_renders_without_decimal() {
        assert_eq!(format_gauge(1.0), "1");
        assert_eq!(format_gauge(-5.0), "-5");
    }

    #[test]
    fn gauge_fractional_renders_with_decimal() {
        assert_eq!(format_gauge(0.5), "0.5");
    }
}
