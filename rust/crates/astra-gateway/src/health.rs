//! Gateway health status — written to a file periodically for monitoring.

use serde::Serialize;

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct HealthStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    pub platforms: Vec<PlatformHealth>,
    pub db_connected: bool,
    pub active_sessions: u64,
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct PlatformHealth {
    pub name: String,
    pub connected: bool,
    pub last_message_secs_ago: Option<u64>,
}

const HEALTH_FILE: &str = "/tmp/astra-gateway.health";

pub fn write_health(status: &HealthStatus) {
    if let Ok(json) = serde_json::to_string_pretty(status) {
        let _ = std::fs::write(HEALTH_FILE, json);
    }
}

pub fn read_health() -> Option<HealthStatus> {
    let content = std::fs::read_to_string(HEALTH_FILE).ok()?;
    serde_json::from_str(&content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_serializes() {
        let status = HealthStatus {
            pid: 12345,
            uptime_secs: 3600,
            platforms: vec![PlatformHealth {
                name: "weixin".into(),
                connected: true,
                last_message_secs_ago: Some(30),
            }],
            db_connected: true,
            active_sessions: 5,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("weixin"));
        assert!(json.contains("12345"));
    }

    #[test]
    fn write_and_read_roundtrip() {
        let status = HealthStatus {
            pid: std::process::id(),
            uptime_secs: 0,
            platforms: vec![],
            db_connected: false,
            active_sessions: 0,
        };
        write_health(&status);
        let read = read_health().unwrap();
        assert_eq!(read.pid, status.pid);
        assert!(!read.db_connected);
    }
}
