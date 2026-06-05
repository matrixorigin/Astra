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

#[cfg(test)]
mod tests {
    use super::*;

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
}
