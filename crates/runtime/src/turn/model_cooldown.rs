//! Model cooldown and typed fallback signals.
//!
//! Provider transport and retries live in `turn::llm::client`; this module is
//! intentionally limited to the process-wide cooldown registry and the
//! structured signal used when the selected model must fall back.

use astra_turn_core::bridge_rate_limit_cooldown::{CooldownReason, PerModelCooldown};
use serde_json::{Value, json};
use std::sync::OnceLock;

pub(crate) fn rate_limit_cooldown() -> &'static PerModelCooldown {
    static COOLDOWN: OnceLock<PerModelCooldown> = OnceLock::new();
    COOLDOWN.get_or_init(PerModelCooldown::new)
}

const FALLBACK_REQUIRED_SOURCE: &str = "llm_fallback_required";

pub(crate) fn fallback_required_error(
    cause: astra_core::ClassifiedError,
    reason: CooldownReason,
) -> astra_core::ClassifiedError {
    cause.with_details_json(
        json!({
            "source": FALLBACK_REQUIRED_SOURCE,
            "reason": reason.as_str(),
        })
        .to_string(),
    )
}

pub(crate) fn fallback_required_reason(
    error: &astra_core::ClassifiedError,
) -> Option<CooldownReason> {
    let details = serde_json::from_str::<Value>(error.details_json.as_deref()?).ok()?;
    if details.get("source").and_then(Value::as_str) != Some(FALLBACK_REQUIRED_SOURCE) {
        return None;
    }
    match details.get("reason").and_then(Value::as_str) {
        Some("rate_limit") => Some(CooldownReason::RateLimit),
        Some("overloaded") => Some(CooldownReason::Overloaded),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_signal_round_trips_without_message_matching() {
        for reason in [CooldownReason::RateLimit, CooldownReason::Overloaded] {
            let error = fallback_required_error(
                astra_core::ClassifiedError::new(astra_core::ErrorKind::RateLimit, "wording"),
                reason,
            );
            assert_eq!(fallback_required_reason(&error), Some(reason));
        }
    }

    #[test]
    fn unrelated_details_do_not_trigger_fallback() {
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::RateLimit,
            "llm_fallback_required overloaded",
        )
        .with_details_json(json!({"source": "provider"}).to_string());
        assert_eq!(fallback_required_reason(&error), None);
    }
}
