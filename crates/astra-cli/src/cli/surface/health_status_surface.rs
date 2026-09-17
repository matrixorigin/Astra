#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthStatusKind {
    Healthy,
    Unhealthy,
}

pub(crate) fn health_status_kind(status: &str) -> HealthStatusKind {
    match status {
        "ok" | "healthy" => HealthStatusKind::Healthy,
        _ => HealthStatusKind::Unhealthy,
    }
}

pub(crate) fn health_status_is_healthy(status: &str) -> bool {
    health_status_kind(status) == HealthStatusKind::Healthy
}

pub(crate) fn health_status_icon(status: &str) -> &'static str {
    if health_status_is_healthy(status) {
        "✓"
    } else {
        "⚠"
    }
}

pub(crate) fn api_probe_is_healthy(status: &str, database: &str) -> bool {
    health_status_is_healthy(status) && database == "connected"
}

pub(crate) fn api_health_body_is_healthy(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(serde_json::Value::as_str)
                .map(health_status_is_healthy)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        HealthStatusKind, api_health_body_is_healthy, api_probe_is_healthy, health_status_icon,
        health_status_kind,
    };

    #[test]
    fn health_status_helpers_classify_known_statuses() {
        assert_eq!(health_status_kind("ok"), HealthStatusKind::Healthy);
        assert_eq!(health_status_kind("healthy"), HealthStatusKind::Healthy);
        assert_eq!(health_status_kind("degraded"), HealthStatusKind::Unhealthy);
        assert_eq!(health_status_icon("ok"), "✓");
        assert_eq!(health_status_icon("error"), "⚠");
    }

    #[test]
    fn api_probe_requires_connected_database() {
        assert!(api_probe_is_healthy("healthy", "connected"));
        assert!(!api_probe_is_healthy("healthy", "disconnected"));
        assert!(!api_probe_is_healthy("degraded", "connected"));
    }

    #[test]
    fn api_health_body_requires_an_explicit_healthy_status() {
        assert!(api_health_body_is_healthy(r#"{"status":"healthy"}"#));
        assert!(api_health_body_is_healthy(r#"{"status":"ok"}"#));
        assert!(!api_health_body_is_healthy(r#"{"status":"degraded"}"#));
        assert!(!api_health_body_is_healthy(r#"{"status":"unhealthy"}"#));
        assert!(!api_health_body_is_healthy(r#"{"database":"connected"}"#));
        assert!(!api_health_body_is_healthy("not-json"));
    }
}
