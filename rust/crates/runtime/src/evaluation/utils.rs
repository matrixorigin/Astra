use axum::{Json, http::{HeaderMap, StatusCode}};

use crate::{ErrorResponse, error_response};
use super::types::QualityTrendPoint;

pub(super) type ServiceResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

pub(crate) fn compute_overall_avg(points: &[QualityTrendPoint]) -> f64 {
    if points.is_empty() {
        return 0.0;
    }
    let (sum, count) = points.iter().fold((0.0_f64, 0_i64), |(s, c), p| {
        (s + p.avg_score * p.count as f64, c + p.count)
    });
    if count == 0 { 0.0 } else { sum / count as f64 }
}

pub(crate) fn compute_calibration_error(mean_confidence: f64, mean_quality: f64) -> f64 {
    (mean_confidence - mean_quality).abs()
}

pub(crate) fn compute_adjustment(calibration_error: f64, bias: f64) -> (f64, String) {
    if calibration_error < 0.05 {
        (1.0, "Well calibrated".into())
    } else if bias > 0.0 {
        let mult = 1.0 - (calibration_error * 0.5).min(0.3);
        (
            mult,
            format!("Overconfident by {:.1}%, reducing", bias * 100.0),
        )
    } else {
        let mult = 1.0 + (calibration_error * 0.5).min(0.3);
        (
            mult,
            format!("Underconfident by {:.1}%, boosting", bias.abs() * 100.0),
        )
    }
}

pub(crate) fn trust_ratio(total: i64, safe: i64) -> f64 {
    if total == 0 {
        1.0
    } else {
        safe as f64 / total as f64
    }
}

pub(crate) fn skill_success_rate(total: i64, success: i64) -> f64 {
    if total == 0 {
        0.0
    } else {
        success as f64 / total as f64
    }
}

pub(super) fn extract_user_id(headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    headers
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Missing X-User-Id header"))
}

pub(super) fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}
