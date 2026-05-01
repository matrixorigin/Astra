use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

// ── Thresholds — single source of truth ──────────────────────────────────────

pub const ZONE_UTIL_HIGH: f64 = 0.80;
pub const ZONE_UTIL_MEDIUM: f64 = 0.60;
pub const RELEVANCE_HIGH: f64 = 0.70;
pub const RELEVANCE_LOW: f64 = 0.40;
pub const POLLUTION_THRESHOLD: f64 = 0.30;
pub const POLLUTION_STATUS_POLLUTED: f64 = 0.25;
pub const POLLUTION_STATUS_NOISY: f64 = 0.10;
pub const QUALITY_GOOD: f64 = 0.60;
pub const QUALITY_DEGRADED: f64 = 0.35;
pub const TREND_CHANGE_PCT: f64 = 0.10;
pub const COMPACTION_DROP_PCT: f64 = 0.80;
pub const COMPACTION_EFFECTIVE_PCT: f64 = 0.25;
pub const DEGRADATION_DELTA: f64 = 0.15;
pub const ZONE_BALANCE_TOLERANCE: f64 = 0.15;
pub const TOKEN_CHAR_RATIO: usize = 2;

// ── Task-type ideal zone weights ─────────────────────────────────────────────

fn ideal_zone_weights(task_type: Option<&str>) -> HashMap<&'static str, f64> {
    match task_type {
        Some("code_gen") => [("code", 0.40), ("history", 0.30), ("memory", 0.15)].into(),
        Some("qa") => [("history", 0.45), ("memory", 0.30), ("code", 0.10)].into(),
        Some("debugging") => [("code", 0.45), ("history", 0.25), ("memory", 0.15)].into(),
        _ => [("history", 0.35), ("code", 0.25), ("memory", 0.20)].into(),
    }
}

// ── Pure analysis result types ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ZoneInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmUsageInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    pub context_window: i64,
    pub utilization: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextHealthResult {
    pub zones: Vec<ZoneInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottleneck: Option<String>,
    pub trend: String,
    pub overall_status: String,
    pub recommendation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_usage: Option<LlmUsageInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_usage_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ZoneBalanceResult {
    pub balanced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub misallocated_zone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_profile: Option<String>,
    pub recommendation: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PollutionResult {
    pub pollution_pct: f64,
    pub status: String,
    pub recommendation: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactionResult {
    pub compactions_detected: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_reduction_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForecastResult {
    pub turns_remaining: Option<i64>,
    pub growth_rate_per_turn: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelevanceQualityResult {
    pub mean: Option<f64>,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryScoreBreakdown {
    pub vector: f64,
    pub keyword: f64,
    pub temporal: f64,
    pub confidence: f64,
}

// ── Pure analysis functions ──────────────────────────────────────────────────

pub fn compute_trend(token_history: &[i64]) -> &'static str {
    if token_history.len() < 2 {
        return "stable";
    }
    let newest = token_history[0] as f64;
    let oldest = token_history[token_history.len() - 1] as f64;
    let delta = newest - oldest;
    let pct = delta / oldest.max(1.0);
    if pct > TREND_CHANGE_PCT {
        "growing"
    } else if pct < -TREND_CHANGE_PCT {
        "shrinking"
    } else {
        "stable"
    }
}

pub fn analyze_context_health(
    budget: &Value,
    total_tokens_history: &[i64],
    llm_prompt_tokens: Option<i64>,
    llm_usage: Option<&Value>,
    context_window: i64,
) -> ContextHealthResult {
    let budget_obj = budget
        .as_object()
        .unwrap_or(&serde_json::Map::new())
        .clone();
    let cw = context_window.max(1) as f64;

    let is_flat = budget_obj.values().all(|v| v.is_number());

    let mut zones = Vec::new();
    let mut bottleneck_zone: Option<String> = None;
    let mut bottleneck_util = 0.0_f64;
    let overall_util;
    let recommendation_base;

    if is_flat {
        let sum_vals: i64 = budget_obj
            .values()
            .filter_map(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .filter(|&v| v > 0)
            .sum();
        let ref_tokens = llm_prompt_tokens.unwrap_or(sum_vals);

        let mut zone_total: i64 = 0;
        for (zone, val) in &budget_obj {
            let tok = match val.as_i64().or_else(|| val.as_f64().map(|f| f as i64)) {
                Some(t) if t > 0 => t,
                _ => continue,
            };
            let share = if ref_tokens > 0 {
                ((tok as f64 / ref_tokens as f64) * 1000.0).round() / 1000.0
            } else {
                0.0
            };
            zones.push(ZoneInfo {
                name: zone.clone(),
                utilization: None,
                status: None,
                tokens: Some(tok),
                share: Some(share),
            });
            zone_total += tok;
        }

        if let Some(lpt) = llm_prompt_tokens
            && lpt > zone_total
        {
            let unmanaged = lpt - zone_total;
            let share = if ref_tokens > 0 {
                ((unmanaged as f64 / ref_tokens as f64) * 1000.0).round() / 1000.0
            } else {
                0.0
            };
            zones.push(ZoneInfo {
                name: "conversation_history_and_tools".into(),
                utilization: None,
                status: None,
                tokens: Some(unmanaged),
                share: Some(share),
            });
        }

        overall_util = ((ref_tokens as f64 / cw) * 1000.0).round() / 1000.0;
        recommendation_base = format!(
            "prompt {} / {} tokens ({:.0}%)",
            ref_tokens,
            context_window,
            overall_util * 100.0,
        );
    } else {
        for (zone, vals) in &budget_obj {
            let (allocated, used) = if let Some(obj) = vals.as_object() {
                let a = obj
                    .get("allocated")
                    .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                    .unwrap_or(0);
                let u = obj
                    .get("used")
                    .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                    .unwrap_or(0);
                (a, u)
            } else if let Some(n) = vals.as_i64().or_else(|| vals.as_f64().map(|f| f as i64)) {
                (n, n)
            } else {
                continue;
            };
            if allocated <= 0 {
                continue;
            }
            let util = ((used as f64 / allocated as f64) * 100.0).round() / 100.0;
            let status = if util >= ZONE_UTIL_HIGH {
                "high"
            } else if util >= ZONE_UTIL_MEDIUM {
                "medium"
            } else {
                "ok"
            };
            zones.push(ZoneInfo {
                name: zone.clone(),
                utilization: Some(util),
                status: Some(status.into()),
                tokens: None,
                share: None,
            });
            if util > bottleneck_util {
                bottleneck_util = util;
                bottleneck_zone = Some(zone.clone());
            }
        }
        overall_util = bottleneck_util;
        recommendation_base = if let Some(ref bz) = bottleneck_zone {
            format!("{bz} zone near limit")
        } else {
            "all zones".into()
        };
    }

    let status = if overall_util >= ZONE_UTIL_HIGH {
        "high"
    } else if overall_util >= ZONE_UTIL_MEDIUM {
        "medium"
    } else {
        "ok"
    };

    let trend = compute_trend(total_tokens_history);

    let recommendation = if overall_util >= ZONE_UTIL_HIGH {
        format!("{recommendation_base} — compaction recommended")
    } else if trend == "growing" && total_tokens_history.len() >= 3 {
        format!("{recommendation_base} — token usage growing, monitor for compaction trigger")
    } else {
        "context healthy".into()
    };

    let (llm_usage_info, llm_usage_note) = if let Some(usage) = llm_usage {
        let prompt = billable_input_from_canonical(usage);
        let completion = usage.get("output_tokens").and_then(|v| v.as_i64());
        let total = usage.get("total_tokens").and_then(|v| v.as_i64());
        let util = ((prompt.unwrap_or(0) as f64 / cw) * 1000.0).round() / 1000.0;
        (
            Some(LlmUsageInfo {
                prompt,
                completion,
                total,
                context_window,
                utilization: util,
            }),
            None,
        )
    } else if let Some(lpt) = llm_prompt_tokens {
        (
            Some(LlmUsageInfo {
                prompt: Some(lpt),
                completion: None,
                total: None,
                context_window,
                utilization: ((lpt as f64 / cw) * 1000.0).round() / 1000.0,
            }),
            None,
        )
    } else {
        (
            None,
            Some(
                "LLM token usage not available for this turn \
                 (first turn or response not yet persisted). \
                 The zones above show only the context-manager-managed portion."
                    .into(),
            ),
        )
    };

    ContextHealthResult {
        zones,
        bottleneck: bottleneck_zone,
        trend: trend.into(),
        overall_status: status.into(),
        recommendation,
        llm_usage: llm_usage_info,
        llm_usage_note,
    }
}

pub fn zone_balance(budget: &Value, task_type: Option<&str>) -> ZoneBalanceResult {
    let budget_obj = match budget.as_object() {
        Some(o) => o,
        None => {
            return ZoneBalanceResult {
                balanced: true,
                misallocated_zone: None,
                matched_profile: None,
                recommendation: "zone balance ok".into(),
            };
        }
    };

    let excluded = ["system", "skills", "reserve"];

    fn alloc_val(v: &Value) -> i64 {
        if let Some(obj) = v.as_object() {
            obj.get("allocated")
                .and_then(|a| a.as_i64().or_else(|| a.as_f64().map(|f| f as i64)))
                .unwrap_or(0)
        } else {
            v.as_i64()
                .or_else(|| v.as_f64().map(|f| f as i64))
                .unwrap_or(0)
        }
    }

    let managed: Vec<(&String, i64)> = budget_obj
        .iter()
        .filter(|(k, v)| !excluded.contains(&k.as_str()) && alloc_val(v) > 0)
        .map(|(k, v)| (k, alloc_val(v)))
        .collect();

    if managed.is_empty() {
        return ZoneBalanceResult {
            balanced: true,
            misallocated_zone: None,
            matched_profile: None,
            recommendation: "zone balance ok".into(),
        };
    }

    let total_alloc: i64 = managed.iter().map(|(_, a)| a).sum();
    let actual: HashMap<&str, f64> = managed
        .iter()
        .map(|(k, a)| (k.as_str(), *a as f64 / total_alloc as f64))
        .collect();

    let profile = task_type.unwrap_or("");
    let ideal = ideal_zone_weights(task_type);
    let matched = if ["code_gen", "qa", "debugging"].contains(&profile) {
        profile
    } else {
        "default"
    };

    let mut worst_zone: Option<&str> = None;
    let mut worst_gap = 0.0_f64;
    for (zone, ideal_pct) in &ideal {
        let gap = (actual.get(zone).copied().unwrap_or(0.0) - ideal_pct).abs();
        if gap > worst_gap {
            worst_gap = gap;
            worst_zone = Some(zone);
        }
    }

    let balanced = worst_gap < ZONE_BALANCE_TOLERANCE;
    if balanced {
        ZoneBalanceResult {
            balanced: true,
            misallocated_zone: None,
            matched_profile: Some(matched.into()),
            recommendation: "zone balance ok".into(),
        }
    } else {
        let wz = worst_zone.unwrap_or("unknown");
        ZoneBalanceResult {
            balanced: false,
            misallocated_zone: Some(wz.into()),
            matched_profile: Some(matched.into()),
            recommendation: format!(
                "{wz} zone allocation off by {:.0}% for {matched} tasks",
                worst_gap * 100.0,
            ),
        }
    }
}

pub fn pollution_ratio(relevance_scores: &HashMap<String, f64>) -> PollutionResult {
    if relevance_scores.is_empty() {
        return PollutionResult {
            pollution_pct: 0.0,
            status: "clean".into(),
            recommendation: "ok".into(),
        };
    }
    let scores: Vec<f64> = relevance_scores.values().copied().collect();
    let low = scores.iter().filter(|&&s| s < POLLUTION_THRESHOLD).count();
    let pct = ((low as f64 / scores.len() as f64) * 100.0).round() / 100.0;
    let status = if pct > POLLUTION_STATUS_POLLUTED {
        "polluted"
    } else if pct > POLLUTION_STATUS_NOISY {
        "noisy"
    } else {
        "clean"
    };
    let recommendation = if status == "polluted" {
        "re-retrieve or raise relevance threshold"
    } else {
        "ok"
    };
    PollutionResult {
        pollution_pct: pct,
        status: status.into(),
        recommendation: recommendation.into(),
    }
}

pub fn compaction_effectiveness(token_history: &[i64]) -> CompactionResult {
    if token_history.len() < 2 {
        return CompactionResult {
            compactions_detected: 0,
            avg_reduction_pct: None,
            status: None,
        };
    }

    let mut compactions = Vec::new();
    for i in 0..token_history.len() - 1 {
        let newer = token_history[i];
        let older = token_history[i + 1];
        if older > 0 && (newer as f64) < (older as f64 * COMPACTION_DROP_PCT) {
            let reduction = ((older - newer) as f64 / older as f64 * 100.0).round() / 100.0;
            compactions.push(reduction);
        }
    }

    if compactions.is_empty() {
        return CompactionResult {
            compactions_detected: 0,
            avg_reduction_pct: None,
            status: Some("none observed".into()),
        };
    }

    let avg = (compactions.iter().sum::<f64>() / compactions.len() as f64 * 100.0).round() / 100.0;
    let status = if avg >= COMPACTION_EFFECTIVE_PCT {
        "effective"
    } else {
        "weak — consider more aggressive compaction"
    };

    CompactionResult {
        compactions_detected: compactions.len(),
        avg_reduction_pct: Some(avg),
        status: Some(status.into()),
    }
}

pub fn compaction_forecast(token_history: &[i64], limit: i64) -> ForecastResult {
    if token_history.len() < 2 {
        return ForecastResult {
            turns_remaining: None,
            growth_rate_per_turn: None,
        };
    }
    let newest = token_history[0] as f64;
    let oldest = token_history[token_history.len() - 1] as f64;
    let growth = (newest - oldest) / (token_history.len() - 1) as f64;
    if growth <= 0.0 {
        return ForecastResult {
            turns_remaining: None,
            growth_rate_per_turn: Some((growth * 10.0).round() / 10.0),
        };
    }
    let remaining = (limit as f64 - newest).max(0.0);
    ForecastResult {
        turns_remaining: Some((remaining / growth).round() as i64),
        growth_rate_per_turn: Some((growth * 10.0).round() / 10.0),
    }
}

pub fn relevance_quality(relevance_scores: &HashMap<String, f64>) -> RelevanceQualityResult {
    if relevance_scores.is_empty() {
        return RelevanceQualityResult {
            mean: None,
            high: 0,
            medium: 0,
            low: 0,
            total: 0,
            quality: None,
        };
    }
    let scores: Vec<f64> = relevance_scores.values().copied().collect();
    let mean = (scores.iter().sum::<f64>() / scores.len() as f64 * 1000.0).round() / 1000.0;
    let high = scores.iter().filter(|&&s| s >= RELEVANCE_HIGH).count();
    let medium = scores
        .iter()
        .filter(|&&s| (RELEVANCE_LOW..RELEVANCE_HIGH).contains(&s))
        .count();
    let low = scores.iter().filter(|&&s| s < RELEVANCE_LOW).count();
    let quality = if mean >= QUALITY_GOOD {
        "good"
    } else if mean >= QUALITY_DEGRADED {
        "degraded"
    } else {
        "poor"
    };
    RelevanceQualityResult {
        mean: Some(mean),
        high,
        medium,
        low,
        total: scores.len(),
        quality: Some(quality.into()),
    }
}

pub fn memory_recall_score(
    content: &str,
    query_terms: &[&str],
    confidence: f64,
    age_days: f64,
) -> MemoryScoreBreakdown {
    let keyword = if query_terms.is_empty() {
        0.0
    } else {
        let content_lower = content.to_lowercase();
        let matches = query_terms
            .iter()
            .filter(|t| content_lower.contains(&t.to_lowercase()))
            .count();
        matches as f64 / query_terms.len() as f64
    };
    let temporal = (1.0 - (age_days / 30.0).min(1.0)).max(0.0);
    let vector = keyword;

    let _final_score =
        ((vector * 0.6 + temporal * 0.2 + confidence * 0.2) * 10000.0).round() / 10000.0;

    MemoryScoreBreakdown {
        vector: (keyword * 10000.0).round() / 10000.0,
        keyword: (keyword * 10000.0).round() / 10000.0,
        temporal: (temporal * 10000.0).round() / 10000.0,
        confidence: (confidence * 10000.0).round() / 10000.0,
    }
}

pub fn memory_recall_final_score(
    content: &str,
    query_terms: &[&str],
    confidence: f64,
    age_days: f64,
) -> f64 {
    let keyword = if query_terms.is_empty() {
        0.0
    } else {
        let content_lower = content.to_lowercase();
        let matches = query_terms
            .iter()
            .filter(|t| content_lower.contains(&t.to_lowercase()))
            .count();
        matches as f64 / query_terms.len() as f64
    };
    let temporal = (1.0 - (age_days / 30.0).min(1.0)).max(0.0);
    let vector = keyword;
    ((vector * 0.6 + temporal * 0.2 + confidence * 0.2) * 10000.0).round() / 10000.0
}

// ── JSON column parsing helper ───────────────────────────────────────────────

pub fn parse_token_usage(raw: &str) -> Option<Value> {
    serde_json::from_str(raw).ok()
}

/// Sum the billable input buckets (`input_tokens` + `cached_input_tokens` +
/// `cache_creation_tokens`) from a canonical [`TokenUsage`]-shaped JSON value.
/// Returns `None` when no recognizable input field is present.
pub fn billable_input_from_canonical(usage: &Value) -> Option<i64> {
    let input = usage.get("input_tokens").and_then(Value::as_i64)?;
    let cached = usage
        .get("cached_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let creation = usage
        .get("cache_creation_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(input + cached + creation)
}

pub fn parse_relevance_scores(raw: &str) -> HashMap<String, f64> {
    let val: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let obj = match val.as_object() {
        Some(o) => o,
        None => return HashMap::new(),
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
        .collect()
}

/// Drift score + categorical level + surfaceable signals for a session.
///
/// Treats the agent's "focus" as drifting from the original user intent to
/// whatever event most recently wrote to the journal. Compares token sets
/// via Jaccard distance — cheap, deterministic, and decent for short
/// English/Chinese text. `drift_score` is in `[0.0, 1.0]`; level bins
/// follow the 0.2 / 0.5 / 0.8 cuts.
///
/// Signals populate from simple heuristics (scope widening, no overlap at
/// all, short-original-long-current) so the LLM has something to read
/// beyond the raw score.
#[must_use]
pub fn compute_drift(
    original_intent: &str,
    current_focus: &str,
) -> (f64, &'static str, Vec<String>) {
    let original_tokens = tokenise(original_intent);
    let current_tokens = tokenise(current_focus);

    if original_tokens.is_empty() || current_tokens.is_empty() {
        // Nothing to compare against — report as aligned and let the LLM
        // decide whether the session has just started.
        return (0.0, "aligned", vec!["insufficient history".into()]);
    }

    let intersection: usize = original_tokens
        .iter()
        .filter(|t| current_tokens.contains(*t))
        .count();
    let union: usize = original_tokens
        .iter()
        .chain(current_tokens.iter())
        .collect::<std::collections::HashSet<_>>()
        .len()
        .max(1);

    let jaccard = (intersection as f64) / (union as f64);
    let drift = (1.0 - jaccard).clamp(0.0, 1.0);

    let level = if drift < 0.2 {
        "aligned"
    } else if drift < 0.5 {
        "mild"
    } else if drift < 0.8 {
        "moderate"
    } else {
        "high"
    };

    let mut signals = Vec::new();
    if intersection == 0 {
        signals.push("no token overlap with original intent".to_string());
    }
    if current_tokens.len() > original_tokens.len().saturating_mul(3) {
        signals.push("current focus is much broader than original scope".to_string());
    }
    if original_tokens.len() > current_tokens.len().saturating_mul(3) {
        signals.push("current focus is much narrower than original scope".to_string());
    }

    (drift, level, signals)
}

/// Lowercase + alphanumeric token segmentation. Deliberately simple; enough
/// for Jaccard on short prompt previews without pulling in a stemmer.
fn tokenise(s: &str) -> std::collections::HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_trend_stable() {
        assert_eq!(compute_trend(&[100, 95]), "stable");
        assert_eq!(compute_trend(&[100]), "stable");
        assert_eq!(compute_trend(&[]), "stable");
    }

    #[test]
    fn test_compute_trend_growing() {
        assert_eq!(compute_trend(&[200, 150, 100]), "growing");
    }

    #[test]
    fn test_compute_trend_shrinking() {
        assert_eq!(compute_trend(&[50, 75, 100]), "shrinking");
    }

    #[test]
    fn test_compute_trend_zero_oldest() {
        assert_eq!(compute_trend(&[100, 0]), "growing");
    }

    #[test]
    fn test_health_flat_budget() {
        let budget = serde_json::json!({"history": 5000, "code": 3000, "memory": 2000});
        let result = analyze_context_health(&budget, &[10000], None, None, 128000);
        assert_eq!(result.overall_status, "ok");
        assert_eq!(result.trend, "stable");
        assert!(result.recommendation.contains("healthy"));
        assert!(result.llm_usage.is_none());
        assert!(result.llm_usage_note.is_some());
    }

    #[test]
    fn test_health_nested_budget_high_util() {
        let budget = serde_json::json!({
            "history": {"allocated": 1000, "used": 900},
            "code": {"allocated": 1000, "used": 200},
        });
        let result = analyze_context_health(&budget, &[1100, 1000], None, None, 128000);
        assert_eq!(result.overall_status, "high");
        assert_eq!(result.bottleneck, Some("history".into()));
        assert!(result.recommendation.contains("compaction"));
    }

    #[test]
    fn test_health_with_llm_usage() {
        let budget = serde_json::json!({"history": 5000});
        let llm = serde_json::json!({
            "input_tokens": 60000,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 500,
            "total_tokens": 60500,
        });
        let result = analyze_context_health(&budget, &[], Some(60000), Some(&llm), 128000);
        assert!(result.llm_usage.is_some());
        let u = result.llm_usage.unwrap();
        assert_eq!(u.prompt, Some(60000));
        assert!(u.utilization > 0.0);
    }

    #[test]
    fn test_zone_balance_default_balanced() {
        let budget = serde_json::json!({"history": 350, "code": 250, "memory": 200, "other": 200});
        let result = zone_balance(&budget, None);
        assert!(result.balanced);
        assert_eq!(result.matched_profile, Some("default".into()));
    }

    #[test]
    fn test_zone_balance_code_gen_misaligned() {
        let budget = serde_json::json!({"history": 800, "code": 100, "memory": 100});
        let result = zone_balance(&budget, Some("code_gen"));
        assert!(!result.balanced);
        assert!(result.misallocated_zone.is_some());
        assert_eq!(result.matched_profile, Some("code_gen".into()));
    }

    #[test]
    fn test_zone_balance_excludes_system() {
        let budget = serde_json::json!({"system": 5000, "history": 350, "code": 250, "memory": 200, "other": 200});
        let result = zone_balance(&budget, None);
        assert!(result.balanced);
    }

    #[test]
    fn test_pollution_clean() {
        let scores: HashMap<String, f64> =
            [("a".into(), 0.8), ("b".into(), 0.7), ("c".into(), 0.9)]
                .into_iter()
                .collect();
        let result = pollution_ratio(&scores);
        assert_eq!(result.status, "clean");
        assert_eq!(result.pollution_pct, 0.0);
    }

    #[test]
    fn test_pollution_polluted() {
        let mut scores = HashMap::new();
        for i in 0..10 {
            let val = if i < 4 { 0.1 } else { 0.8 };
            scores.insert(format!("item_{i}"), val);
        }
        let result = pollution_ratio(&scores);
        assert_eq!(result.status, "polluted");
        assert!(result.pollution_pct > POLLUTION_STATUS_POLLUTED);
    }

    #[test]
    fn test_pollution_empty() {
        let result = pollution_ratio(&HashMap::new());
        assert_eq!(result.status, "clean");
        assert_eq!(result.pollution_pct, 0.0);
    }

    #[test]
    fn test_compaction_none() {
        let result = compaction_effectiveness(&[100, 110, 120]);
        assert_eq!(result.compactions_detected, 0);
        assert_eq!(result.status, Some("none observed".into()));
    }

    #[test]
    fn test_compaction_detected() {
        let result = compaction_effectiveness(&[50, 100]);
        assert_eq!(result.compactions_detected, 1);
        assert!(result.avg_reduction_pct.unwrap() >= 0.25);
        assert_eq!(result.status, Some("effective".into()));
    }

    #[test]
    fn test_compaction_too_short() {
        let result = compaction_effectiveness(&[100]);
        assert_eq!(result.compactions_detected, 0);
        assert!(result.avg_reduction_pct.is_none());
    }

    #[test]
    fn test_forecast_growing() {
        let result = compaction_forecast(&[150, 125, 100], 200);
        assert_eq!(result.turns_remaining, Some(2));
        assert!(result.growth_rate_per_turn.unwrap() > 0.0);
    }

    #[test]
    fn test_forecast_shrinking() {
        let result = compaction_forecast(&[80, 100], 200);
        assert_eq!(result.turns_remaining, None);
        assert!(result.growth_rate_per_turn.unwrap() < 0.0);
    }

    #[test]
    fn test_forecast_insufficient_data() {
        let result = compaction_forecast(&[100], 200);
        assert_eq!(result.turns_remaining, None);
        assert_eq!(result.growth_rate_per_turn, None);
    }

    #[test]
    fn test_relevance_good() {
        let scores: HashMap<String, f64> =
            [("a".into(), 0.9), ("b".into(), 0.8), ("c".into(), 0.7)]
                .into_iter()
                .collect();
        let result = relevance_quality(&scores);
        assert_eq!(result.quality, Some("good".into()));
        assert_eq!(result.high, 3);
        assert_eq!(result.total, 3);
    }

    #[test]
    fn test_relevance_poor() {
        let scores: HashMap<String, f64> =
            [("a".into(), 0.1), ("b".into(), 0.2)].into_iter().collect();
        let result = relevance_quality(&scores);
        assert_eq!(result.quality, Some("poor".into()));
        assert_eq!(result.low, 2);
    }

    #[test]
    fn test_relevance_empty() {
        let result = relevance_quality(&HashMap::new());
        assert_eq!(result.mean, None);
        assert_eq!(result.total, 0);
        assert_eq!(result.quality, None);
    }

    #[test]
    fn test_recall_all_terms_match() {
        let breakdown = memory_recall_score("hello world test", &["hello", "world"], 0.9, 1.0);
        assert!((breakdown.keyword - 1.0).abs() < 0.001);
        assert!(breakdown.temporal > 0.9);
    }

    #[test]
    fn test_recall_no_terms() {
        let breakdown = memory_recall_score("anything", &[], 0.5, 10.0);
        assert!((breakdown.keyword - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_recall_old_memory() {
        let breakdown = memory_recall_score("test content", &["test"], 0.5, 60.0);
        assert!((breakdown.temporal - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_recall_final_score() {
        let score = memory_recall_final_score("hello world", &["hello", "world"], 0.8, 0.0);
        assert!((score - 0.96).abs() < 0.001);
    }

    #[test]
    fn test_recall_partial_match() {
        let score = memory_recall_final_score("hello foo", &["hello", "world"], 0.5, 15.0);
        assert!((score - 0.5).abs() < 0.001);
    }

    // ── compute_drift ──────────────────────────────────────────────────

    #[test]
    fn compute_drift_identical_is_fully_aligned() {
        let (score, level, signals) = compute_drift("list all rust files", "list all rust files");
        assert!((score - 0.0).abs() < f64::EPSILON);
        assert_eq!(level, "aligned");
        assert!(signals.is_empty());
    }

    #[test]
    fn compute_drift_completely_unrelated_is_high() {
        let (score, level, signals) =
            compute_drift("list all rust files", "deploy the website to production");
        assert!(score > 0.8, "expected high drift score, got {score}");
        assert_eq!(level, "high");
        assert!(signals.iter().any(|s| s.contains("no token overlap")));
    }

    #[test]
    fn compute_drift_empty_inputs_return_aligned_with_signal() {
        let (score, level, signals) = compute_drift("", "anything");
        assert!((score - 0.0).abs() < f64::EPSILON);
        assert_eq!(level, "aligned");
        assert!(signals.iter().any(|s| s.contains("insufficient")));
    }

    #[test]
    fn compute_drift_broader_scope_emits_signal() {
        // original has 2 tokens, current has 7 → >3x → "broader" signal
        let (_score, _level, signals) = compute_drift(
            "list files",
            "list all rust files in src and tests and examples",
        );
        assert!(
            signals.iter().any(|s| s.contains("broader")),
            "expected broader-scope signal in: {signals:?}"
        );
    }

    #[test]
    fn compute_drift_level_bins() {
        // Partial overlap: 2/4 = 0.5 → drift 0.5 → moderate.
        let (score, level, _) = compute_drift("alpha beta gamma delta", "alpha beta epsilon zeta");
        assert!(
            (0.5..0.8).contains(&score),
            "score out of moderate bin: {score}"
        );
        assert_eq!(level, "moderate");

        // Small overlap: 1/5 = 0.2 → drift 0.8 → high
        let (score, level, _) = compute_drift("alpha beta gamma", "alpha delta epsilon");
        assert!(score >= 0.8);
        assert_eq!(level, "high");
    }

    #[test]
    fn compute_drift_score_is_always_in_unit_interval() {
        for (a, b) in [
            ("a b c", "a b c"),
            ("a", "z"),
            ("", ""),
            ("alpha beta", "alpha beta gamma delta"),
        ] {
            let (score, _, _) = compute_drift(a, b);
            assert!(
                (0.0..=1.0).contains(&score),
                "score {score} out of [0,1] for ({a:?}, {b:?})"
            );
        }
    }
}
