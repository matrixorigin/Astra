use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingMetricsPlan {
    pub confidence: f64,
    pub threshold: f64,
    pub record_fallback: bool,
    pub record_cache_hit: bool,
    pub record_correction: bool,
    pub efficiency_ratio: Option<f64>,
}

#[allow(clippy::too_many_arguments)]
pub fn build_routing_metrics_plan(
    confidence: f64,
    threshold: f64,
    matched_by: &str,
    tier: i64,
    has_tier1: bool,
    forced: Option<&str>,
    intent: &str,
    estimated_tokens: i64,
    full_question_tokens: i64,
) -> RoutingMetricsPlan {
    RoutingMetricsPlan {
        confidence,
        threshold,
        record_fallback: matched_by == "fallback",
        record_cache_hit: tier == 0 && !has_tier1,
        record_correction: forced == Some("question"),
        efficiency_ratio: if !intent.is_empty() && intent != "question" && full_question_tokens > 0
        {
            Some(1.0 - estimated_tokens as f64 / full_question_tokens as f64)
        } else {
            None
        },
    }
}
