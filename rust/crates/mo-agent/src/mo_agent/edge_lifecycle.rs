//! Cloud edge registry + heartbeat (Phase 3). See `docs/design/multi-agent-cloud-runtime.md` §5.5.

use std::time::Duration;

use astra_thin_client::{
    EdgeHeartbeatRequest, EdgeRegisterRequest, ThinClient, ThinClientError,
    edge_register_with_capabilities,
};
use crossterm::style::Stylize;

use crate::chat_stream::edge_executor_instance_id;

/// When `MO_EDGE_REGISTRY` is `0`, `false`, or `off`, skip register and heartbeat.
pub fn edge_cloud_registry_enabled() -> bool {
    !matches!(
        std::env::var("MO_EDGE_REGISTRY").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

fn heartbeat_period() -> Option<Duration> {
    let secs: u64 = std::env::var("MO_EDGE_HEARTBEAT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

fn enrich_register_body(body: &mut EdgeRegisterRequest) {
    if body.hostname.is_none() {
        body.hostname = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("COMPUTERNAME").ok());
    }
    if body.worktree_path.is_none()
        && let Ok(cwd) = std::env::current_dir()
    {
        body.worktree_path = cwd.to_str().map(String::from);
    }
}

pub async fn register_edge_once(api: &ThinClient, token: &str) -> Result<(), ThinClientError> {
    if !edge_cloud_registry_enabled() {
        return Ok(());
    }
    let transport_id = edge_executor_instance_id();
    let mut body = edge_register_with_capabilities(transport_id);
    enrich_register_body(&mut body);
    api.post_agents_edge_register(Some(token), Some(transport_id), &body)
        .await?;
    Ok(())
}

async fn send_heartbeat(api: &ThinClient, token: &str) -> Result<(), ThinClientError> {
    if !edge_cloud_registry_enabled() {
        return Ok(());
    }
    let id = edge_executor_instance_id();
    let hb = EdgeHeartbeatRequest {
        edge_agent_id: id.to_string(),
    };
    api.post_agents_edge_heartbeat(Some(token), Some(id), &hb)
        .await?;
    Ok(())
}

pub fn spawn_edge_heartbeat(api: ThinClient, token: String) -> Option<tokio::task::JoinHandle<()>> {
    let period = heartbeat_period()?;
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.tick().await;
        loop {
            interval.tick().await;
            let _ = send_heartbeat(&api, &token).await;
        }
    }))
}

/// Register with the cloud (best-effort) and start a background heartbeat task.
/// Returns `None` when registry is disabled, register failed, or heartbeat interval is `0`.
pub async fn register_and_start_heartbeat(
    api: &ThinClient,
    token: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    if !edge_cloud_registry_enabled() {
        return None;
    }
    if let Err(e) = register_edge_once(api, token).await {
        eprintln!(
            "{}",
            format!("  · Edge registry skipped ({e}). Chat and tools still work.").dim()
        );
        return None;
    }
    eprintln!(
        "{}",
        "  · Edge node registered with cloud (heartbeat in background)".dim()
    );
    spawn_edge_heartbeat(api.clone(), token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_thin_client::MO_EDGE_ID_HEADER;
    use serial_test::serial;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `serial_test::serial` on callers avoids concurrent env access from other tests.
    fn env_set(key: &str, value: &str) {
        // SAFETY: only used from `#[serial]` tests in this module.
        unsafe { std::env::set_var(key, value) }
    }

    fn env_remove(key: &str) {
        // SAFETY: only used from `#[serial]` tests in this module.
        unsafe { std::env::remove_var(key) }
    }

    #[test]
    #[serial]
    fn edge_cloud_registry_enabled_respects_env() {
        let prev = std::env::var("MO_EDGE_REGISTRY").ok();
        env_remove("MO_EDGE_REGISTRY");
        assert!(edge_cloud_registry_enabled());
        env_set("MO_EDGE_REGISTRY", "0");
        assert!(!edge_cloud_registry_enabled());
        env_set("MO_EDGE_REGISTRY", "false");
        assert!(!edge_cloud_registry_enabled());
        env_set("MO_EDGE_REGISTRY", "off");
        assert!(!edge_cloud_registry_enabled());
        match &prev {
            Some(v) => env_set("MO_EDGE_REGISTRY", v),
            None => env_remove("MO_EDGE_REGISTRY"),
        }
    }

    #[test]
    #[serial]
    fn heartbeat_period_parsing() {
        let prev = std::env::var("MO_EDGE_HEARTBEAT_SECS").ok();
        env_remove("MO_EDGE_HEARTBEAT_SECS");
        assert_eq!(heartbeat_period(), Some(Duration::from_secs(120)));
        env_set("MO_EDGE_HEARTBEAT_SECS", "0");
        assert_eq!(heartbeat_period(), None);
        env_set("MO_EDGE_HEARTBEAT_SECS", "30");
        assert_eq!(heartbeat_period(), Some(Duration::from_secs(30)));
        match &prev {
            Some(v) => env_set("MO_EDGE_HEARTBEAT_SECS", v),
            None => env_remove("MO_EDGE_HEARTBEAT_SECS"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn register_disabled_skips_http() {
        let prev_reg = std::env::var("MO_EDGE_REGISTRY").ok();
        env_set("MO_EDGE_REGISTRY", "0");
        let api = ThinClient::new("http://127.0.0.1:1", None).expect("url");
        let r = register_edge_once(&api, "token").await;
        assert!(r.is_ok());
        match &prev_reg {
            Some(v) => env_set("MO_EDGE_REGISTRY", v),
            None => env_remove("MO_EDGE_REGISTRY"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn register_edge_once_hits_wiremock() {
        env_remove("MO_EDGE_REGISTRY");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .and(header_exists("authorization"))
            .and(header_exists(MO_EDGE_ID_HEADER))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("url");
        register_edge_once(&api, "test-bearer")
            .await
            .expect("register");

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
        assert!(body.get("edge_agent_id").and_then(|v| v.as_str()).is_some());
        assert!(body.get("capabilities").is_some());
    }

    #[tokio::test]
    #[serial]
    async fn register_enriches_hostname_from_env() {
        let prev_host = std::env::var("HOSTNAME").ok();
        let prev_reg = std::env::var("MO_EDGE_REGISTRY").ok();
        env_set("HOSTNAME", "unit-test-host");
        env_remove("MO_EDGE_REGISTRY");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let api = ThinClient::new(&server.uri(), None).expect("url");
        register_edge_once(&api, "t").await.expect("register");

        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
        assert_eq!(
            body.get("hostname").and_then(|v| v.as_str()),
            Some("unit-test-host")
        );

        match &prev_host {
            Some(v) => env_set("HOSTNAME", v),
            None => env_remove("HOSTNAME"),
        }
        match &prev_reg {
            Some(v) => env_set("MO_EDGE_REGISTRY", v),
            None => env_remove("MO_EDGE_REGISTRY"),
        }
    }
}
