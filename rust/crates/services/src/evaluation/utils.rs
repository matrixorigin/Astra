use axum::{Json, http::StatusCode};

use super::types::QualityTrendPoint;
use astra_core::{ErrorResponse, error_response};

pub type ServiceResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

pub fn not_implemented(message: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    error_response(StatusCode::NOT_IMPLEMENTED, message)
}

pub fn compute_overall_avg(points: &[QualityTrendPoint]) -> f64 {
    if points.is_empty() {
        return 0.0;
    }
    let (sum, count) = points.iter().fold((0.0_f64, 0_i64), |(s, c), p| {
        (s + p.avg_score * p.count as f64, c + p.count)
    });
    if count == 0 { 0.0 } else { sum / count as f64 }
}

pub fn compute_calibration_error(mean_confidence: f64, mean_quality: f64) -> f64 {
    (mean_confidence - mean_quality).abs()
}

pub fn compute_adjustment(calibration_error: f64, bias: f64) -> (f64, String) {
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

pub fn trust_ratio(total: i64, safe: i64) -> f64 {
    if total == 0 {
        1.0
    } else {
        safe as f64 / total as f64
    }
}

pub fn skill_success_rate(total: i64, success: i64) -> f64 {
    if total == 0 {
        0.0
    } else {
        success as f64 / total as f64
    }
}

pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(date: &str, avg_score: f64, count: i64) -> QualityTrendPoint {
        QualityTrendPoint {
            date: date.into(),
            avg_score,
            avg_score_interval: astra_core::confidence::ConfidenceInterval::exact(avg_score),
            count,
            model: None,
        }
    }

    // ──────────────────────────────────────────────────────────
    // compute_overall_avg
    // ──────────────────────────────────────────────────────────

    #[test]
    fn overall_avg_empty() {
        assert_eq!(compute_overall_avg(&[]), 0.0);
    }

    #[test]
    fn overall_avg_single() {
        let pts = vec![point("2025-01-01", 0.8, 10)];
        assert!((compute_overall_avg(&pts) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn overall_avg_weighted() {
        // (0.8 * 10 + 0.6 * 30) / 40 = (8 + 18) / 40 = 0.65
        let pts = vec![point("d1", 0.8, 10), point("d2", 0.6, 30)];
        assert!((compute_overall_avg(&pts) - 0.65).abs() < 1e-9);
    }

    #[test]
    fn overall_avg_zero_count() {
        let pts = vec![point("d1", 0.9, 0)];
        assert_eq!(compute_overall_avg(&pts), 0.0);
    }

    // ──────────────────────────────────────────────────────────
    // compute_calibration_error
    // ──────────────────────────────────────────────────────────

    #[test]
    fn calibration_error_perfect() {
        assert_eq!(compute_calibration_error(0.8, 0.8), 0.0);
    }

    #[test]
    fn calibration_error_positive_gap() {
        assert!((compute_calibration_error(0.9, 0.7) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn calibration_error_negative_gap() {
        assert!((compute_calibration_error(0.5, 0.8) - 0.3).abs() < 1e-9);
    }

    // ──────────────────────────────────────────────────────────
    // compute_adjustment
    // ──────────────────────────────────────────────────────────

    #[test]
    fn adjustment_well_calibrated() {
        let (mult, reason) = compute_adjustment(0.01, 0.01);
        assert_eq!(mult, 1.0);
        assert!(reason.contains("Well calibrated"));
    }

    #[test]
    fn adjustment_overconfident() {
        let (mult, reason) = compute_adjustment(0.2, 0.2);
        assert!(mult < 1.0);
        assert!(reason.contains("Overconfident"));
    }

    #[test]
    fn adjustment_underconfident() {
        let (mult, reason) = compute_adjustment(0.2, -0.2);
        assert!(mult > 1.0);
        assert!(reason.contains("Underconfident"));
    }

    #[test]
    fn adjustment_overconfident_capped() {
        // Large calibration error: mult = 1.0 - min(0.9*0.5, 0.3) = 1.0 - 0.3 = 0.7
        let (mult, _) = compute_adjustment(0.9, 0.9);
        assert!((mult - 0.7).abs() < 1e-9);
    }

    #[test]
    fn adjustment_underconfident_capped() {
        // Large calibration error: mult = 1.0 + min(0.9*0.5, 0.3) = 1.0 + 0.3 = 1.3
        let (mult, _) = compute_adjustment(0.9, -0.9);
        assert!((mult - 1.3).abs() < 1e-9);
    }

    // ──────────────────────────────────────────────────────────
    // trust_ratio
    // ──────────────────────────────────────────────────────────

    #[test]
    fn trust_ratio_zero_total() {
        assert_eq!(trust_ratio(0, 0), 1.0);
    }

    #[test]
    fn trust_ratio_all_safe() {
        assert_eq!(trust_ratio(100, 100), 1.0);
    }

    #[test]
    fn trust_ratio_half() {
        assert!((trust_ratio(100, 50) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn trust_ratio_none_safe() {
        assert_eq!(trust_ratio(100, 0), 0.0);
    }

    // ──────────────────────────────────────────────────────────
    // skill_success_rate
    // ──────────────────────────────────────────────────────────

    #[test]
    fn skill_success_rate_zero_total() {
        assert_eq!(skill_success_rate(0, 0), 0.0);
    }

    #[test]
    fn skill_success_rate_all_success() {
        assert_eq!(skill_success_rate(50, 50), 1.0);
    }

    #[test]
    fn skill_success_rate_partial() {
        assert!((skill_success_rate(100, 75) - 0.75).abs() < 1e-9);
    }

    // ──────────────────────────────────────────────────────────
    // now_iso
    // ──────────────────────────────────────────────────────────

    #[test]
    fn now_iso_format() {
        let s = now_iso();
        // Should look like "2025-01-01T00:00:00"
        assert!(s.contains('T'));
        assert_eq!(s.len(), 19);
    }

    // ──────────────────────────────────────────────────────────
    // not_implemented
    // ──────────────────────────────────────────────────────────

    #[test]
    fn not_implemented_returns_501() {
        let (status, _body) = not_implemented("nope");
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }
}
